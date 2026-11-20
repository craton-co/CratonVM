// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Declarative x86-64 instruction patterns for the JIT backend.
//!
//! # Why this module exists (P0: "create declarative instruction patterns")
//!
//! `x64.rs` selects and encodes instructions in the same breath: every
//! emitter is a hand-written function that decides the REX byte, the opcode,
//! the ModRM/SIB bytes and the immediate width inline, and the *selection*
//! decisions (shrink `MOV r64, 0` to `XOR r32, r32`, prefer `imm8` over
//! `imm32`, elide a self-move) are `if` statements scattered through those
//! functions. Nothing states, in one place, what an instruction *is*: which
//! operands it accepts, which registers it clobbers, what it does to the
//! flags, which bases it cannot address, or what it costs.
//!
//! This module is that statement. [`PATTERNS`] is a table of [`Pattern`]
//! rows; each row carries
//!
//! * the **operation** ([`Op`]) and **operand type** ([`Ty`]),
//! * the **operand kinds** it accepts ([`OpKind`] for destination, source and
//!   a third "extra" slot used by condition codes and parametric opcodes),
//! * the **immediate form** ([`ImmForm`]) and, for memory operands, the
//!   **addressing/displacement policy** ([`DispPolicy`]) — which is nothing
//!   but a choice between the three entry points of [`super::disp`],
//! * the **clobbers**, the **flag effect** ([`FlagEffect`]) and any
//!   **fixed-register constraints** ([`FixedReg`]) such as "shift count must
//!   be in CL" or "IDIV reads RDX:RAX",
//! * the **operand constraints** ([`Constraint`]) that make an encoding
//!   illegal rather than merely unusual, and
//! * the **cost** ([`Cost`]) and the **encoding template** ([`Enc`]).
//!
//! [`select`] is a matcher over that table: given a [`Req`] it finds every row
//! whose operation, type and operand kinds match, discards the rows whose
//! constraints the operands violate, encodes the survivors and returns the
//! shortest one. Because the shrink chains are expressed as *separate rows*
//! (`mov_r64_imm0_xor`, `mov_r32_imm32_zx`, `mov_r64_imm32`, `mov_r64_imm64`) rather than as `if`
//! statements, the selector reproduces `emit_mov_imm64`'s four-way shrink by
//! construction instead of by imitation.
//!
//! # What makes the table trustworthy
//!
//! Every row names the `x64.rs` emitter it reproduces in its `emitter` field,
//! and the test module asserts **byte-for-byte equality** between the row and
//! that emitter across a register matrix that includes R8–R15 and the
//! RSP/RBP/R12/R13 addressing special cases. A row that encodes differently
//! from the emitter it claims to reproduce is a bug in the row, not an
//! improvement — the tests are written so that such a row fails.
//!
//! The single-pass emitters in `x64.rs` do not call this module; migrating
//! them onto it is a separate change (see `docs/jit/instruction-patterns.md`
//! for the migration order). The IR tier does: `ir_lower::build_mir_plan`
//! runs [`select_block`], and under `CRATONVM_JIT=ir-isel-emit` its
//! `Rule::AluReg` tiles are encoded through [`select`] and are the emitted
//! bytes (`ir-isel-verify` compares them against the per-opcode arm instead).
//! A wrong row here is therefore wrong code behind that flag, not only a
//! failing test.
//!
//! # Displacements
//!
//! No row re-derives displacement narrowing. [`DispPolicy`] has exactly three
//! values and they map one-to-one onto [`Disp::encode_for_base`],
//! [`Disp::encode32`] and the "always emit an explicit byte" shape that
//! `emit_test_mem8_imm8` needs. The RBP/R13 (no `mod=00` form) and RSP/R12
//! (SIB required) rules are asked of [`base_requires_displacement`] and
//! [`base_requires_sib`], never restated here.
//!
//! # Error-handling discipline
//!
//! Inherited from the parent module: no panics, no `unwrap`/`expect` outside
//! test code. Every fallible operation returns [`SelError`].
//!
//! (Do not spell the test-gate attribute literally in prose above production
//! code: `jit/tests/panic_free_compile_ratchet.rs` stops scanning a file at
//! its first occurrence, and this sentence used to end the scan of this whole
//! file at line 71.)

use super::{
    base_requires_sib, is_extended, rex, Disp, DispOutOfRange, GPR64_NAMES, RAX, RCX, RDX, RSP,
    XMM_NAMES,
};

// ---------------------------------------------------------------------------
// Operations, types and operand kinds
// ---------------------------------------------------------------------------

/// The machine operation a pattern performs.
///
/// This is the *selector's* vocabulary, not x86's: `Op::Mov` covers every
/// move whose semantics are "copy a value", including the `XOR r,r` zeroing
/// idiom, because a selector asked for `MOV r64, 0` should be free to answer
/// with the shorter encoding.
///
/// That answer WRITES EFLAGS. A request made between a flag producer and its
/// consumer (`CMP` .. `Jcc`/`SETcc`/`CMOVcc`) must say so with
/// [`Req::preserving_flags`], which refuses every flag-writing row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Copy a value (register, immediate or memory) into the destination.
    Mov,
    /// Sign-extending narrow load / register widen.
    Movsx,
    /// Zero-extending narrow load.
    Movzx,
    /// `MOVSXD` — 32-bit source sign-extended into a 64-bit destination.
    Movsxd,
    /// Address computation (`LEA`), which touches no memory and no flags.
    Lea,
    Add,
    Sub,
    And,
    Or,
    /// Bitwise exclusive-or.
    ///
    /// Distinct from the `XOR r,r` *zeroing idiom*, which is a
    /// [`Op::Mov`] row (`mov_r64_imm0_xor`): a selector asking for
    /// "put 0 in this register" and a selector asking for "exclusive-or these
    /// two values" are different requests that happen to share an opcode, and
    /// merging them would let the zeroing row answer a real `xor`.
    Xor,
    Cmp,
    Test,
    /// Signed multiply, two-operand form.
    Imul,
    /// Signed divide by the named register; dividend is the implicit
    /// `RDX:RAX` / `EDX:EAX` pair.
    Idiv,
    /// Sign-extend `RAX` into `RDX:RAX` (`CQO`) or `EAX` into `EDX:EAX`
    /// (`CDQ`) — the mandatory prologue to [`Op::Idiv`].
    SignExtendAcc,
    /// Logical shift left.
    Shl,
    /// Logical shift right.
    Shr,
    /// Arithmetic (sign-propagating) shift right.
    Sar,
    /// Conditional move; the condition arrives in the `extra` operand.
    Cmov,
    Push,
    Pop,
    Ret,
    /// Unconditional relative jump.
    Jmp,
    /// Conditional relative jump; the condition arrives in the `extra`
    /// operand as the second opcode byte.
    Jcc,
    /// 64-bit move between an XMM register and a GPR or memory.
    Movq,
    /// Scalar double move between XMM registers.
    Movsd,
    /// Scalar single move between XMM registers.
    Movss,
    /// Packed XOR, used only as the XMM zeroing idiom.
    Pxor,
    /// Scalar double square root.
    Sqrtsd,
    /// The parametric 32-bit ALU family: the opcode byte itself is an
    /// operand (`emit_alu_r32_r32`).
    Alu,
}

/// Operand type. Names the width and domain the pattern operates on; for
/// extending loads it names the source width. The row encodes the destination
/// width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    /// No typed operand (`RET`, `JMP`).
    Void,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

/// The kind of thing an operand slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// Slot unused by this pattern.
    None,
    /// General-purpose register, numbered 0..=15.
    Gpr,
    /// XMM register, numbered 0..=15.
    Xmm,
    /// Memory operand described by [`Mem`].
    Mem,
    /// Immediate value.
    Imm,
    /// Relative branch displacement (emitted as a 32-bit field, normally
    /// zero and patched later).
    Rel32,
    /// Condition-code byte (the `cc` in `CMOVcc` / `Jcc`).
    Cc,
    /// A raw opcode byte supplied by the caller (the parametric ALU family).
    OpByte,
}

/// A scaled index register in a memory operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Index {
    /// Index register number, 0..=15. Never `RSP` — a SIB index field of
    /// `0b100` with `REX.X` clear means "no index".
    pub reg: u8,
    /// Scale factor: 1, 2, 4 or 8.
    pub scale: u8,
}

/// A memory operand: `[base + index*scale + disp]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mem {
    /// Base register number, 0..=15.
    pub base: u8,
    /// Optional scaled index.
    pub index: Option<Index>,
    /// Byte displacement. Encoded through [`super::disp`], never narrowed
    /// here.
    pub disp: i64,
    /// The displacement must occupy a fixed 32-bit field.
    ///
    /// Set this when the instruction's length has to be known before the
    /// displacement's final value is — a patched site, a RIP-relative
    /// operand — or when reproducing one of `x64.rs`'s `*_disp32` emitters.
    /// The matcher uses it to choose between the otherwise-identical
    /// smallest-form and forced-32-bit rows.
    pub force_disp32: bool,
}

impl Mem {
    /// `[base + disp]`, smallest legal displacement form.
    pub fn base_disp(base: u8, disp: i64) -> Mem {
        Mem {
            base,
            index: None,
            disp,
            force_disp32: false,
        }
    }

    /// `[base + disp]` with the displacement pinned to a 32-bit field.
    pub fn base_disp32(base: u8, disp: i64) -> Mem {
        Mem {
            base,
            index: None,
            disp,
            force_disp32: true,
        }
    }

    /// `[base + index*scale + disp]`, smallest legal displacement form.
    pub fn base_index(base: u8, index: u8, scale: u8, disp: i64) -> Mem {
        Mem {
            base,
            index: Some(Index { reg: index, scale }),
            disp,
            force_disp32: false,
        }
    }
}

/// One operand as handed to the matcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    None,
    Gpr(u8),
    Xmm(u8),
    Mem(Mem),
    Imm(i64),
    Rel32(i32),
    Cc(u8),
    OpByte(u8),
}

impl Operand {
    /// The [`OpKind`] this operand satisfies.
    pub fn kind(self) -> OpKind {
        match self {
            Operand::None => OpKind::None,
            Operand::Gpr(_) => OpKind::Gpr,
            Operand::Xmm(_) => OpKind::Xmm,
            Operand::Mem(_) => OpKind::Mem,
            Operand::Imm(_) => OpKind::Imm,
            Operand::Rel32(_) => OpKind::Rel32,
            Operand::Cc(_) => OpKind::Cc,
            Operand::OpByte(_) => OpKind::OpByte,
        }
    }
}

/// The flattened operand values an encoder works from.
///
/// [`Req::args`] projects a request into this shape; the encoder never looks
/// at [`Operand`] again, so a pattern's encoding depends only on numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Destination register number (GPR or XMM).
    pub dst: u8,
    /// Source register number (GPR or XMM).
    pub src: u8,
    /// Memory operand, when the pattern has one.
    pub mem: Mem,
    /// Immediate / relative displacement value.
    pub imm: i64,
    /// Condition-code or parametric opcode byte.
    pub op_byte: u8,
}

/// A selection request: "give me the best encoding of this operation".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Req {
    pub op: Op,
    pub ty: Ty,
    pub dst: Operand,
    pub src: Operand,
    /// Third slot, used by the condition-code and parametric-opcode families.
    pub extra: Operand,
    /// The flags are LIVE across this instruction: [`select`] skips every row
    /// whose [`FlagEffect`] writes them. `false` (the default) lets it pick the
    /// shortest row, which for `MOV r, 0` is the flag-writing `XOR r32, r32`.
    pub preserve_flags: bool,
}

impl Req {
    /// A two-operand request with no `extra` slot.
    pub fn new(op: Op, ty: Ty, dst: Operand, src: Operand) -> Req {
        Req {
            op,
            ty,
            dst,
            src,
            extra: Operand::None,
            preserve_flags: false,
        }
    }

    /// The same request with the `extra` slot filled in.
    pub fn with_extra(self, extra: Operand) -> Req {
        Req { extra, ..self }
    }

    /// The same request, for a program point where EFLAGS are live (between a
    /// compare and the branch, `SETcc` or `CMOVcc` that reads it). Any emitter
    /// that materialises a value there must ask this way: the selector is
    /// otherwise flag-blind by design and answers `MOV r, 0` with `XOR`
    /// (page `r11-x64gen-isel-select-ignores-flag-effects`). An op every row of
    /// which writes the flags (`Add`, `Cmp`, ...) then has no answer, which is
    /// the correct refusal.
    pub fn preserving_flags(self) -> Req {
        Req {
            preserve_flags: true,
            ..self
        }
    }

    /// Flatten this request into the encoder's operand view.
    pub fn args(&self) -> Args {
        let mut a = Args::default();
        apply(&mut a, self.dst, true);
        apply(&mut a, self.src, false);
        apply(&mut a, self.extra, false);
        a
    }
}

fn apply(a: &mut Args, o: Operand, dst_slot: bool) {
    match o {
        Operand::None => {}
        Operand::Gpr(r) | Operand::Xmm(r) => {
            if dst_slot {
                a.dst = r;
            } else {
                a.src = r;
            }
        }
        Operand::Mem(m) => a.mem = m,
        Operand::Imm(v) => a.imm = v,
        // Widening i32 -> i64: every rel32 target fits the immediate field.
        Operand::Rel32(v) => a.imm = v as i64,
        Operand::Cc(c) | Operand::OpByte(c) => a.op_byte = c,
    }
}

// ---------------------------------------------------------------------------
// Encoding template
// ---------------------------------------------------------------------------

/// The opcode bytes of a pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    /// A single opcode byte.
    One(u8),
    /// A `0F xx` two-byte opcode.
    Two(u8),
    /// A single opcode byte with the destination register number folded into
    /// its low three bits (`50+rd`, `58+rd`, `B8+rd`). The base byte must
    /// have those bits clear.
    PlusReg(u8),
    /// The opcode byte is an operand (`Args::op_byte`).
    OperandOne,
    /// `0F xx` where `xx` is an operand — `CMOVcc`, `Jcc`.
    OperandTwo,
}

/// Which operand supplies the ModRM `reg` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegF {
    Dst,
    Src,
    /// An opcode extension `/n`; `n` must be 0..=7 and contributes no REX.R.
    Ext(u8),
}

/// Which operand supplies the ModRM `r/m` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RmF {
    /// Register-direct (`mod=11`) naming the destination.
    RegDst,
    /// Register-direct (`mod=11`) naming the source.
    RegSrc,
    /// The memory operand in [`Args::mem`].
    Mem,
    /// No ModRM byte at all; the register (if any) lives in the opcode.
    None,
}

/// When the REX prefix byte is emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RexMode {
    /// Never — the pattern has no register operand that can be extended and
    /// no 64-bit operand size (`RET`, `JMP rel32`, `CDQ`).
    Never,
    /// Only when at least one of W/R/X/B is set.
    OnDemand,
    /// Always, because REX.W is part of the encoding.
    Always,
}

/// Which of [`super::disp`]'s three entry points resolves this pattern's
/// displacement.
///
/// There is deliberately no fourth value: a pattern that needs some other
/// narrowing rule is a pattern that re-derives displacement encoding, which
/// is the hazard `disp.rs` exists to remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispPolicy {
    /// The pattern has no memory operand.
    NotApplicable,
    /// [`Disp::encode_for_base`] — smallest legal form, with the RBP/R13
    /// "no `mod=00`" rule applied.
    Smallest,
    /// [`Disp::encode32`] — always `mod=10` with a four-byte field.
    Force32,
    /// [`Disp::encode_for_base`], then promote [`Disp::None`] to an explicit
    /// `disp8` of zero.
    ///
    /// `emit_test_mem8_imm8` has always emitted a displacement byte, even for
    /// the zero-displacement safepoint poll. Shrinking it to `mod=00` would
    /// move every following instruction by one byte under the branch patcher,
    /// so the shape is preserved rather than optimised.
    AtLeast8,
}

/// The immediate field, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImmForm {
    /// No immediate field. (A pattern may still *match* on an immediate
    /// operand without encoding it — the `XOR` zeroing idiom does.)
    None,
    /// One byte, read back by the CPU as signed.
    Imm8,
    /// One byte, read back as an unsigned mask or count.
    ImmU8,
    /// Four bytes, little-endian, sign-extended by the instruction.
    Imm32,
    /// Four bytes, little-endian, ZERO-extended by the instruction (`MOV r32,
    /// imm32` writing a 32-bit register). The value must fit `u32`.
    ImmU32,
    /// Eight bytes, little-endian.
    Imm64,
}

impl ImmForm {
    /// Number of bytes this immediate occupies.
    pub fn byte_len(self) -> usize {
        match self {
            ImmForm::None => 0,
            ImmForm::Imm8 | ImmForm::ImmU8 => 1,
            ImmForm::Imm32 | ImmForm::ImmU32 => 4,
            ImmForm::Imm64 => 8,
        }
    }
}

/// An encoding-time simplification the pattern is allowed to perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peephole {
    /// None.
    No,
    /// Emit nothing when destination and source name the same register.
    /// Both `emit_mov_reg_reg` and `emit_mov_r64_r64` do this; a coalesced
    /// live range routinely produces such a copy.
    ElideWhenDstEqSrc,
}

/// The byte-level encoding template of a pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enc {
    /// Mandatory legacy prefix (`66`, `F2`, `F3`), emitted before REX.
    pub prefix: Option<u8>,
    /// REX.W — 64-bit operand size.
    pub rex_w: bool,
    /// When the REX byte is emitted.
    pub rex: RexMode,
    /// Opcode bytes.
    pub opcode: Opcode,
    /// Whether a ModRM byte follows the opcode.
    pub modrm: bool,
    /// Source of the ModRM `reg` field.
    pub reg: RegF,
    /// Source of the ModRM `r/m` field.
    pub rm: RmF,
}

impl Enc {
    /// Functional-update base: a one-byte opcode, register-direct, no
    /// prefix, no REX unless a register is extended.
    pub const BASE: Enc = Enc {
        prefix: None,
        rex_w: false,
        rex: RexMode::OnDemand,
        opcode: Opcode::One(0x90),
        modrm: true,
        reg: RegF::Dst,
        rm: RmF::RegSrc,
    };
}

// ---------------------------------------------------------------------------
// Constraints, effects, cost
// ---------------------------------------------------------------------------

/// A property the operands must have for this encoding to be legal.
///
/// A violated constraint is an [`Err`], never a silently different
/// instruction: that is the whole point of stating them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constraint {
    /// The immediate must fit a signed 8-bit field.
    ImmFitsI8,
    /// The immediate must fit an unsigned 8-bit field.
    ImmFitsU8,
    /// The immediate must fit a signed 32-bit field.
    ImmFitsI32,
    /// The immediate must be exactly zero (the zeroing idiom).
    ImmIsZero,
    /// The immediate must *not* fit a signed 32-bit field — otherwise a
    /// shorter row covers it.
    ImmNeedsI64,
    /// The shift count must be 0..=63.
    ShiftCountFits64,
    /// The base register must not be RSP/R12: this encoding emits no SIB
    /// byte, and `r/m=100` would be read as "a SIB byte follows".
    NoSibBase,
    /// This encoding has no index field.
    NoIndex,
    /// RSP can never be a SIB index: index `0b100` with `REX.X` clear means
    /// "no index". (R12 *is* usable as an index in 64-bit mode, because
    /// `REX.X` distinguishes it; `x64.rs`'s `emit_string_decode_char`
    /// comment is stricter than the architecture requires.)
    IndexNotRsp,
    /// The base must not be RBP/R13, which have no `mod=00` form.
    BaseNotRbpClass,
    /// ModRM `reg` and `r/m` must name the same register (`XOR r,r`,
    /// `TEST r,r`, `PXOR x,x`).
    SameRegister,
    /// The condition byte must be a `CMOVcc` opcode, `0x40..=0x4F`.
    CcIsCmov,
    /// The condition byte must be the second byte of a two-byte `Jcc`,
    /// `0x80..=0x8F`.
    CcIsJcc,
}

/// What a pattern does to the arithmetic flags (OF SF ZF AF PF CF).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlagEffect {
    /// The instruction overwrites the flags.
    pub writes: bool,
    /// The instruction consumes the flags.
    pub reads: bool,
}

impl FlagEffect {
    /// Flags untouched — moves, `LEA`, `PUSH`/`POP`, `CQO`.
    pub const NONE: FlagEffect = FlagEffect {
        writes: false,
        reads: false,
    };
    /// Flags written.
    pub const W: FlagEffect = FlagEffect {
        writes: true,
        reads: false,
    };
    /// Flags read — `CMOVcc`, `Jcc`.
    pub const R: FlagEffect = FlagEffect {
        writes: false,
        reads: true,
    };
}

/// The role a fixed register plays in a pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The variable shift count, which x86 only reads from CL.
    ShiftCount,
    /// An implicitly read register that is not an operand slot.
    ImplicitSrc,
    /// An implicitly written register that is not an operand slot.
    ImplicitDst,
}

/// A register this pattern requires in a specific place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedReg {
    pub role: Role,
    pub reg: u8,
}

/// How a pattern touches memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemEffect {
    /// Touches no memory.
    No,
    /// Reads memory.
    Read,
    /// Writes memory.
    Write,
    /// Reads and writes memory (`PUSH`/`POP` through the stack pointer).
    ReadWrite,
}

/// Static cost model for a pattern.
///
/// `bytes` is the *minimum* encoded length (the operand-independent floor);
/// the selector compares actual encoded lengths and only falls back to
/// `uops`/`latency` to break a tie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cost {
    pub bytes: u8,
    pub uops: u8,
    pub latency: u8,
}

impl Cost {
    pub const fn new(bytes: u8, uops: u8, latency: u8) -> Cost {
        Cost {
            bytes,
            uops,
            latency,
        }
    }
}

/// Whether [`select`] may choose a pattern on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The matcher considers this row.
    Auto,
    /// Reachable only by name, through [`pattern`] / [`encode_named`].
    ///
    /// Used for rows that duplicate another row's semantics with a different
    /// encoding (`MOV r/m64, r64` versus `MOV r64, r/m64`) or that
    /// deliberately decline an available optimisation (the non-shrinking
    /// `MOV r64, imm64` an inline-cache site needs so its length is fixed).
    Explicit,
}

// ---------------------------------------------------------------------------
// The pattern
// ---------------------------------------------------------------------------

/// One declarative instruction pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// Stable identifier, unique across [`PATTERNS`].
    pub name: &'static str,
    /// The `x64.rs` emitter (or inline byte literal) this row reproduces
    /// byte-for-byte. Empty when the row has no counterpart yet.
    pub emitter: &'static str,
    pub op: Op,
    pub ty: Ty,
    /// Operand kind accepted in the destination slot.
    pub dst: OpKind,
    /// Operand kind accepted in the source slot.
    pub src: OpKind,
    /// Operand kind accepted in the third slot.
    pub extra: OpKind,
    pub enc: Enc,
    pub disp: DispPolicy,
    pub imm: ImmForm,
    pub peephole: Peephole,
    pub constraints: &'static [Constraint],
    /// GPR numbers written besides the destination.
    pub clobbers: &'static [u8],
    pub fixed: &'static [FixedReg],
    pub flags: FlagEffect,
    pub mem: MemEffect,
    pub cost: Cost,
    pub mode: Mode,
}

const NO_CONSTRAINTS: &[Constraint] = &[];
const NO_CLOBBERS: &[u8] = &[];
const NO_FIXED: &[FixedReg] = &[];
const CLOBBERS_RSP: &[u8] = &[RSP];
const CLOBBERS_RAX_RDX: &[u8] = &[RAX, RDX];
const CLOBBERS_RDX: &[u8] = &[RDX];
const FIXED_SHIFT_CL: &[FixedReg] = &[FixedReg {
    role: Role::ShiftCount,
    reg: RCX,
}];
const FIXED_DIVIDEND: &[FixedReg] = &[
    FixedReg {
        role: Role::ImplicitSrc,
        reg: RAX,
    },
    FixedReg {
        role: Role::ImplicitSrc,
        reg: RDX,
    },
];
const FIXED_ACC: &[FixedReg] = &[FixedReg {
    role: Role::ImplicitSrc,
    reg: RAX,
}];

impl Pattern {
    /// Functional-update base for the table: a flag-free, memory-free,
    /// constraint-free two-register operation.
    pub const BASE: Pattern = Pattern {
        name: "",
        emitter: "",
        op: Op::Mov,
        ty: Ty::I64,
        dst: OpKind::Gpr,
        src: OpKind::Gpr,
        extra: OpKind::None,
        enc: Enc::BASE,
        disp: DispPolicy::NotApplicable,
        imm: ImmForm::None,
        peephole: Peephole::No,
        constraints: NO_CONSTRAINTS,
        clobbers: NO_CLOBBERS,
        fixed: NO_FIXED,
        flags: FlagEffect::NONE,
        mem: MemEffect::No,
        cost: Cost::new(3, 1, 1),
        mode: Mode::Auto,
    };
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a pattern could not be selected or encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelError {
    /// The operands violate a constraint the pattern declares.
    Constraint {
        pattern: &'static str,
        constraint: Constraint,
    },
    /// The displacement has no x86-64 encoding at all.
    Disp(DispOutOfRange),
    /// The immediate does not fit the field the pattern declares.
    Immediate {
        pattern: &'static str,
        form: ImmForm,
        value: i64,
    },
    /// A SIB scale that is not 1, 2, 4 or 8.
    BadScale { scale: u8 },
    /// A register number outside 0..=15.
    BadRegister { pattern: &'static str, reg: u8 },
    /// No row in the table covers this request.
    NoPattern { op: Op, ty: Ty },
    /// [`pattern`] was asked for a name the table does not contain.
    UnknownPattern,
    /// A memory operand named no base register.
    ///
    /// `[index*scale + disp32]` is a legal x86-64 operand, but [`Mem`] has no
    /// way to express "no base" — its `base` is a `u8`, not an `Option<u8>` —
    /// so an address with only an index has no representation here. Refusing
    /// is the fail-closed direction; the caller falls back to a shift.
    NoBaseRegister,
}

impl std::fmt::Display for SelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelError::Constraint {
                pattern,
                constraint,
            } => write!(
                f,
                "pattern `{pattern}` rejects the operands: {constraint:?}"
            ),
            SelError::Disp(e) => write!(f, "{e}"),
            SelError::Immediate {
                pattern,
                form,
                value,
            } => write!(
                f,
                "pattern `{pattern}` cannot encode immediate {value} as {form:?}"
            ),
            SelError::BadScale { scale } => {
                write!(f, "SIB scale {scale} is not 1, 2, 4 or 8")
            }
            SelError::BadRegister { pattern, reg } => {
                write!(f, "pattern `{pattern}` got register number {reg} (max 15)")
            }
            SelError::NoPattern { op, ty } => {
                write!(f, "no instruction pattern for {op:?}/{ty:?}")
            }
            SelError::UnknownPattern => write!(f, "no such instruction pattern"),
            SelError::NoBaseRegister => {
                write!(f, "memory operand has no base register")
            }
        }
    }
}

impl std::error::Error for SelError {}

impl From<DispOutOfRange> for SelError {
    fn from(e: DispOutOfRange) -> Self {
        SelError::Disp(e)
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// An encoded instruction plus the offsets of its patchable fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// The instruction bytes.
    pub bytes: Vec<u8>,
    /// Offset of the displacement field within `bytes`, when there is one.
    pub disp_offset: Option<usize>,
    /// Offset of the immediate field within `bytes`, when there is one.
    ///
    /// For the `Jcc`/`JMP` rows this is exactly the patch site the
    /// corresponding `x64.rs` emitter returns to its caller.
    pub imm_offset: Option<usize>,
}

impl Encoded {
    fn empty() -> Encoded {
        Encoded {
            bytes: Vec::new(),
            disp_offset: None,
            imm_offset: None,
        }
    }
}

fn scale_bits(scale: u8) -> Result<u8, SelError> {
    match scale {
        1 => Ok(0b00),
        2 => Ok(0b01),
        4 => Ok(0b10),
        8 => Ok(0b11),
        other => Err(SelError::BadScale { scale: other }),
    }
}

impl Pattern {
    /// Does this pattern address memory?
    pub fn is_memory(&self) -> bool {
        matches!(self.enc.rm, RmF::Mem)
    }

    /// Check every declared constraint against `a`.
    ///
    /// Called by [`Pattern::encode`] before a single byte is produced, so a
    /// rejected operand set can never leave a partial instruction behind.
    pub fn check(&self, a: &Args) -> Result<(), SelError> {
        if matches!(self.dst, OpKind::Gpr | OpKind::Xmm) && a.dst > 15 {
            return Err(SelError::BadRegister {
                pattern: self.name,
                reg: a.dst,
            });
        }
        if matches!(self.src, OpKind::Gpr | OpKind::Xmm) && a.src > 15 {
            return Err(SelError::BadRegister {
                pattern: self.name,
                reg: a.src,
            });
        }
        if self.is_memory() {
            if a.mem.base > 15 {
                return Err(SelError::BadRegister {
                    pattern: self.name,
                    reg: a.mem.base,
                });
            }
            if let Some(ix) = a.mem.index {
                if ix.reg > 15 {
                    return Err(SelError::BadRegister {
                        pattern: self.name,
                        reg: ix.reg,
                    });
                }
                scale_bits(ix.scale)?;
                // Structural, not per-row: SIB `index=100` with REX.X clear
                // means "no index", so an RSP index would silently become
                // `[base + disp]` on EVERY memory row, not only the ones that
                // remembered to declare `Constraint::IndexNotRsp` (the two
                // `LEA` rows, which `IrAddr` lowering feeds, did not). Reported
                // as that constraint so a row that does declare it answers
                // exactly as before.
                if ix.reg == RSP {
                    return Err(SelError::Constraint {
                        pattern: self.name,
                        constraint: Constraint::IndexNotRsp,
                    });
                }
            }
        }
        for c in self.constraints {
            let ok = match *c {
                Constraint::ImmFitsI8 => i8::try_from(a.imm).is_ok(),
                Constraint::ImmFitsU8 => u8::try_from(a.imm).is_ok(),
                Constraint::ImmFitsI32 => i32::try_from(a.imm).is_ok(),
                Constraint::ImmIsZero => a.imm == 0,
                Constraint::ImmNeedsI64 => i32::try_from(a.imm).is_err(),
                Constraint::ShiftCountFits64 => (0..=63).contains(&a.imm),
                Constraint::NoSibBase => !base_requires_sib(a.mem.base),
                Constraint::NoIndex => a.mem.index.is_none(),
                Constraint::IndexNotRsp => match a.mem.index {
                    Some(ix) => ix.reg != RSP,
                    None => true,
                },
                Constraint::BaseNotRbpClass => !super::base_requires_displacement(a.mem.base),
                Constraint::SameRegister => a.dst == a.src,
                Constraint::CcIsCmov => (0x40..=0x4F).contains(&a.op_byte),
                Constraint::CcIsJcc => (0x80..=0x8F).contains(&a.op_byte),
            };
            if !ok {
                return Err(SelError::Constraint {
                    pattern: self.name,
                    constraint: *c,
                });
            }
        }
        Ok(())
    }

    /// Resolve this pattern's displacement through [`super::disp`].
    pub fn resolve_disp(&self, a: &Args) -> Result<Disp, SelError> {
        let d = match self.disp {
            DispPolicy::Force32 => Disp::encode32(a.mem.disp),
            DispPolicy::Smallest | DispPolicy::AtLeast8 => {
                Disp::encode_for_base(a.mem.disp, a.mem.base)
            }
            // A pattern with no memory operand never reaches here; answer
            // with the empty form rather than inventing an error path.
            DispPolicy::NotApplicable => Ok(Disp::None),
        }
        .map_err(SelError::Disp)?;
        if matches!(self.disp, DispPolicy::AtLeast8) && matches!(d, Disp::None) {
            return Ok(Disp::Disp8(0));
        }
        Ok(d)
    }

    /// Encode this pattern for `a`.
    pub fn encode(&self, a: &Args) -> Result<Encoded, SelError> {
        self.check(a)?;
        let mut out = Encoded::empty();
        if matches!(self.peephole, Peephole::ElideWhenDstEqSrc) && a.dst == a.src {
            return Ok(out);
        }

        if let Some(p) = self.enc.prefix {
            out.bytes.push(p);
        }

        let reg_num: u8 = match self.enc.reg {
            RegF::Dst => a.dst,
            RegF::Src => a.src,
            RegF::Ext(n) => n,
        };
        let (rm_num, index_num) = match self.enc.rm {
            RmF::RegDst => (a.dst, 0u8),
            // The register, if there is one, lives in the opcode byte and
            // still needs REX.B. A pattern with no register operand at all
            // (`CQO`, `RET`) must contribute no REX bit — otherwise an
            // unrelated `Args::dst` would change the encoding.
            RmF::None => {
                if matches!(self.dst, OpKind::Gpr | OpKind::Xmm) {
                    (a.dst, 0u8)
                } else {
                    (0u8, 0u8)
                }
            }
            RmF::RegSrc => (a.src, 0u8),
            RmF::Mem => (
                a.mem.base,
                match a.mem.index {
                    Some(ix) => ix.reg,
                    None => 0u8,
                },
            ),
        };

        // An opcode extension `/n` occupies the ModRM `reg` field but is not a
        // register, so it never contributes REX.R.
        let rex_r = !matches!(self.enc.reg, RegF::Ext(_)) && is_extended(reg_num);
        let rex_byte = rex(
            self.enc.rex_w,
            rex_r,
            is_extended(index_num),
            is_extended(rm_num),
        );
        // For 8-bit GPR moves (`mov_m8_r8*`), registers 4..8 (`SPL/BPL/SIL/DIL`)
        // require a REX prefix (even bare `0x40`) so the CPU does not decode
        // them as `AH/CH/DH/BH`.
        let byte_reg_needs_rex = self.op == Op::Mov
            && self.ty == Ty::I8
            && ((matches!(self.dst, OpKind::Gpr) && (4..8).contains(&a.dst))
                || (matches!(self.src, OpKind::Gpr) && (4..8).contains(&a.src)));
        match self.enc.rex {
            RexMode::Never => {}
            RexMode::OnDemand => {
                if rex_byte != 0x40 || byte_reg_needs_rex {
                    out.bytes.push(rex_byte);
                }
            }
            RexMode::Always => out.bytes.push(rex_byte),
        }

        match self.enc.opcode {
            Opcode::One(b) => out.bytes.push(b),
            Opcode::Two(b) => {
                out.bytes.push(0x0F);
                out.bytes.push(b);
            }
            // The base byte's low three bits are zero (asserted by
            // `plus_reg_opcodes_have_room_for_the_register`), so `|` and `+`
            // agree — `|` cannot overflow.
            Opcode::PlusReg(b) => out.bytes.push(b | (a.dst & 7)),
            Opcode::OperandOne => out.bytes.push(a.op_byte),
            Opcode::OperandTwo => {
                out.bytes.push(0x0F);
                out.bytes.push(a.op_byte);
            }
        }

        if self.enc.modrm {
            if matches!(self.enc.rm, RmF::Mem) {
                let d = self.resolve_disp(a)?;
                let needs_sib = a.mem.index.is_some() || base_requires_sib(a.mem.base);
                let rm_field = if needs_sib { 0b100 } else { a.mem.base & 7 };
                out.bytes.push(d.modrm(reg_num, rm_field));
                if needs_sib {
                    let (idx_field, scale_field) = match a.mem.index {
                        Some(ix) => (ix.reg & 7, scale_bits(ix.scale)?),
                        // index=100 with REX.X clear is the "no index" form.
                        None => (0b100u8, 0b00u8),
                    };
                    out.bytes
                        .push((scale_field << 6) | (idx_field << 3) | (a.mem.base & 7));
                }
                if d.byte_len() > 0 {
                    out.disp_offset = Some(out.bytes.len());
                    let (bytes, len) = d.bytes();
                    out.bytes.extend_from_slice(&bytes[..len]);
                }
            } else {
                out.bytes.push(0xC0 | ((reg_num & 7) << 3) | (rm_num & 7));
            }
        }

        if !matches!(self.imm, ImmForm::None) {
            out.imm_offset = Some(out.bytes.len());
        }
        match self.imm {
            ImmForm::None => {}
            ImmForm::Imm8 => {
                let v = i8::try_from(a.imm).map_err(|_| SelError::Immediate {
                    pattern: self.name,
                    form: ImmForm::Imm8,
                    value: a.imm,
                })?;
                // Cast: x86-64 immediate encoding — the value is already
                // range-checked as a *signed* i8, so this is the encoding,
                // not a narrowing.
                out.bytes.push(v as u8);
            }
            ImmForm::ImmU8 => {
                let v = u8::try_from(a.imm).map_err(|_| SelError::Immediate {
                    pattern: self.name,
                    form: ImmForm::ImmU8,
                    value: a.imm,
                })?;
                out.bytes.push(v);            }
            ImmForm::Imm32 => {
                let v = i32::try_from(a.imm).map_err(|_| SelError::Immediate {
                    pattern: self.name,
                    form: ImmForm::Imm32,
                    value: a.imm,
                })?;
                out.bytes.extend_from_slice(&v.to_le_bytes());            }
            ImmForm::ImmU32 => {
                let v = u32::try_from(a.imm).map_err(|_| SelError::Immediate {
                    pattern: self.name,
                    form: ImmForm::ImmU32,
                    value: a.imm,
                })?;
                out.bytes.extend_from_slice(&v.to_le_bytes());            }
            ImmForm::Imm64 => out.bytes.extend_from_slice(&a.imm.to_le_bytes()),        }

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// Every declarative pattern known to the backend.
///
/// Ordering is by family, not by cost: [`select`] ranks candidates by the
/// length they actually encode to.
pub static PATTERNS: &[Pattern] = &[
    // ── GPR moves ────────────────────────────────────────────────────────
    Pattern {
        name: "mov_r64_r64",
        emitter: "emit_mov_reg_reg",
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x8B),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        peephole: Peephole::ElideWhenDstEqSrc,
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_r64_r64_store_form",
        emitter: "emit_mov_r64_r64",
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        peephole: Peephole::ElideWhenDstEqSrc,
        mode: Mode::Explicit,
        ..Pattern::BASE
    },
    Pattern {
        name: "movsxd_r64_r32",
        emitter: "x64.rs inline `rex_w(); [0x63, 0xC0]` (arith 32-bit widen)",
        op: Op::Movsxd,
        ty: Ty::I32,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x63),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        ..Pattern::BASE
    },
    // Allocated integer operands, including spilled right operands.
    Pattern {
        name: "mov_r32_r32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (32-bit copy)",
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        peephole: Peephole::ElideWhenDstEqSrc,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "add_r32_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Add,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x03),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(2, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "add_r64_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Add,
        ty: Ty::I64,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x03),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r32_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Sub,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x2B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(2, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r64_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Sub,
        ty: Ty::I64,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x2B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "and_r32_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::And,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x23),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(2, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "and_r64_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::And,
        ty: Ty::I64,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x23),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r32_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Or,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x0B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(2, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r64_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Or,
        ty: Ty::I64,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x0B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r32_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Xor,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x33),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(2, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r64_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory operand)",
        op: Op::Xor,
        ty: Ty::I64,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x33),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "and_r32_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate operand)",
        op: Op::And,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "and_r64_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate operand)",
        op: Op::And,
        ty: Ty::I64,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r32_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate operand)",
        op: Op::Or,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(1),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r64_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate operand)",
        op: Op::Or,
        ty: Ty::I64,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(1),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r32_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate operand)",
        op: Op::Xor,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(6),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r64_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate operand)",
        op: Op::Xor,
        ty: Ty::I64,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(6),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r64_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate operand)",
        op: Op::Xor,
        ty: Ty::I64,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(6),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r32_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory multiply)",
        op: Op::Imul,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::Two(0xAF),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 6),
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r64_m",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (memory multiply)",
        op: Op::Imul,
        ty: Ty::I64,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xAF),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(4, 1, 6),
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r32_r32_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate multiply)",
        op: Op::Imul,
        ty: Ty::I32,
        extra: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x6B),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 3),
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r32_r32_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate multiply)",
        op: Op::Imul,
        ty: Ty::I32,
        extra: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0x69),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(6, 1, 3),
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r64_r64_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate multiply)",
        op: Op::Imul,
        ty: Ty::I64,
        extra: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x6B),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 3),
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r64_r64_imm32",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate multiply)",
        op: Op::Imul,
        ty: Ty::I64,
        extra: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x69),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 3),
        ..Pattern::BASE
    },
    Pattern {
        name: "shl_r32_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate shift)",
        op: Op::Shl,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0xC1),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ShiftCountFits64],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "shl_r64_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate shift)",
        op: Op::Shl,
        ty: Ty::I64,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xC1),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ShiftCountFits64],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "shr_r32_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate shift)",
        op: Op::Shr,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0xC1),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ShiftCountFits64],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sar_r32_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate shift)",
        op: Op::Sar,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::One(0xC1),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ShiftCountFits64],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sar_r64_imm8",
        emitter: "ir_lower::direct_alu_bytes at 140ef45cce (immediate shift)",
        op: Op::Sar,
        ty: Ty::I64,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xC1),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ShiftCountFits64],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    // ── GPR immediates ───────────────────────────────────────────────────
    Pattern {
        name: "mov_r64_imm0_xor",
        emitter: "emit_xor_reg_self",
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x31),
            reg: RegF::Dst,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        constraints: &[Constraint::ImmIsZero],
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_r64_imm32",
        emitter: "emit_mov_imm32_sx",
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xC7),
            reg: RegF::Ext(0),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    // `MOV r32, imm32` — writing the 32-bit register zero-extends, so for any
    // value in `0..=u32::MAX` the 64-bit result is exact. 5 bytes (6 with
    // REX.B). `select` prefers it over `mov_r64_imm32` (7) for a positive
    // value and over `mov_r64_imm64` (10) for the `u32` range; zero stays
    // `XOR` (2). The `ImmU32` field refuses a negative or wider value.
    Pattern {
        name: "mov_r32_imm32_zx",
        emitter: "emit_mov_r32_imm32_zx",
        src: OpKind::Imm,
        enc: Enc {
            rex_w: false,
            rex: RexMode::OnDemand,
            opcode: Opcode::PlusReg(0xB8),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        imm: ImmForm::ImmU32,
        cost: Cost::new(5, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_r64_imm64",
        emitter: "emit_mov_imm64",
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::PlusReg(0xB8),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        imm: ImmForm::Imm64,
        constraints: &[Constraint::ImmNeedsI64],
        cost: Cost::new(10, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_r64_imm64_full",
        emitter: "emit_mov_imm64_full",
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::PlusReg(0xB8),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        imm: ImmForm::Imm64,
        cost: Cost::new(10, 1, 1),
        // Never selected automatically: the inline-cache sites need a fixed
        // ten-byte shape whose imm64 they can rewrite in place, so shrinking
        // it would break the patcher, not just the size.
        mode: Mode::Explicit,
        ..Pattern::BASE
    },
    // ── GPR loads and stores ─────────────────────────────────────────────
    Pattern {
        name: "mov_r64_m",
        emitter: "emit_load_local / emit_load_caller_arg",
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x8B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_r64_m_disp32",
        emitter: "emit_mov_r64_mem_disp32",
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x8B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Read,
        cost: Cost::new(7, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m_r64",
        emitter: "emit_store_local / emit_mov_rsp_disp_from_reg",
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Write,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m_r64_disp32",
        emitter: "emit_mov_mem_disp32_r64",
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Write,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_r32_m",
        emitter: "lower_allocated_array_length",
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            opcode: Opcode::One(0x8B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        cost: Cost::new(2, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_r32_m_disp32",
        emitter: "emit_mov_r32_mem_disp32 / emit_stack_bang_load",
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            opcode: Opcode::One(0x8B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Read,
        cost: Cost::new(6, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m32_imm32",
        emitter: "emit_mov_dword_mem_disp32_imm32",
        ty: Ty::I32,
        dst: OpKind::Mem,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0xC7),
            reg: RegF::Ext(0),
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        mem: MemEffect::Write,
        cost: Cost::new(10, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m32_imm32_smallest",
        emitter: "emit_gpr_array_elem_store",
        ty: Ty::I32,
        dst: OpKind::Mem,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0xC7),
            reg: RegF::Ext(0),
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        mem: MemEffect::Write,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m8_r8",
        emitter: "emit_gpr_array_elem_store",
        ty: Ty::I8,
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            opcode: Opcode::One(0x88),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Write,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m8_r8_disp32",
        emitter: "emit_compact_prim_field_store",
        ty: Ty::I8,
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            opcode: Opcode::One(0x88),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Write,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m16_r16",
        emitter: "emit_gpr_array_elem_store",
        ty: Ty::I16,
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            prefix: Some(0x66),
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Write,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m16_r16_disp32",
        emitter: "emit_compact_prim_field_store",
        ty: Ty::I16,
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            prefix: Some(0x66),
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Write,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m32_r32",
        emitter: "emit_gpr_array_elem_store",
        ty: Ty::I32,
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Write,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m32_r32_disp32",
        emitter: "emit_compact_prim_field_store",
        ty: Ty::I32,
        dst: OpKind::Mem,
        src: OpKind::Gpr,
        enc: Enc {
            opcode: Opcode::One(0x89),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Write,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_m8_imm8",
        emitter: "ir_lower::lower_data_node (Op::ArrayStore Byte boolean tag check)",
        op: Op::Cmp,
        ty: Ty::I8,
        dst: OpKind::Mem,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x80),
            reg: RegF::Ext(7),
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::AtLeast8,
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ImmFitsU8, Constraint::NoIndex],
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(4, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "mov_m8_index_imm8",
        emitter: "emit_mov_mem8_indexed_imm8",
        ty: Ty::I8,
        dst: OpKind::Mem,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0xC6),
            reg: RegF::Ext(0),
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ImmFitsU8, Constraint::IndexNotRsp],
        mem: MemEffect::Write,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "movsxd_r64_m_disp32",
        emitter: "emit_movsxd_r64_mem_disp32",
        op: Op::Movsxd,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x63),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Read,
        cost: Cost::new(7, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movsx_r64_m8_disp32",
        emitter: "emit_movx_r64_mem_disp32(8, signed)",
        op: Op::Movsx,
        ty: Ty::I8,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xBE),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Read,
        cost: Cost::new(8, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movzx_r64_m8_disp32",
        emitter: "emit_movx_r64_mem_disp32(8, unsigned)",
        op: Op::Movzx,
        ty: Ty::I8,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xB6),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Read,
        cost: Cost::new(8, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movsx_r64_m16_disp32",
        emitter: "emit_movx_r64_mem_disp32(16, signed)",
        op: Op::Movsx,
        ty: Ty::I16,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xBF),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Read,
        cost: Cost::new(8, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movzx_r64_m16_disp32",
        emitter: "emit_movx_r64_mem_disp32(16, unsigned)",
        op: Op::Movzx,
        ty: Ty::I16,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xB7),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        mem: MemEffect::Read,
        cost: Cost::new(8, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movzx_r32_m8",
        emitter: "emit_str_char_load_to",
        op: Op::Movzx,
        ty: Ty::I8,
        src: OpKind::Mem,
        enc: Enc {
            opcode: Opcode::Two(0xB6),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movzx_r32_m16",
        emitter: "emit_str_char_load_to",
        op: Op::Movzx,
        ty: Ty::I16,
        src: OpKind::Mem,
        enc: Enc {
            opcode: Opcode::Two(0xB7),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movsxd_r64_m",
        emitter: "allocated array operands",
        op: Op::Movsxd,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x63),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        flags: FlagEffect::NONE,
        cost: Cost::new(3, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movsx_r64_m8",
        emitter: "allocated array operands",
        op: Op::Movsx,
        ty: Ty::I8,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xBE),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        flags: FlagEffect::NONE,
        cost: Cost::new(4, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movsx_r64_m16",
        emitter: "allocated array operands",
        op: Op::Movsx,
        ty: Ty::I16,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xBF),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        flags: FlagEffect::NONE,
        cost: Cost::new(4, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_r32_m",
        emitter: "allocated array operands",
        op: Op::Cmp,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: false,
            opcode: Opcode::One(0x3B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 4),
        ..Pattern::BASE
    },
    // ── Address computation ──────────────────────────────────────────────
    Pattern {
        name: "lea_r64_m",
        emitter: "emit_lea_frame_slot",
        op: Op::Lea,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x8D),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        // LEA reads no memory: it computes the effective address only.
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    // 32-bit `LEA`, the form real Java `int` arithmetic asks for.
    //
    // Anchored: `x64.rs`'s small-multiply fast path already emits exactly this
    // row's bytes — `emit_imul_const` (`x64/arith.rs:790`, `:797`, `:804`)
    // writes `8D 04 40` / `8D 04 80` / `8D 04 C0`, i.e. `LEA EAX, [RAX+RAX*n]`
    // with no REX prefix. `rex_w: false` plus `RexMode::IfNeeded` reproduces
    // that byte-for-byte for registers 0-7 and adds REX only where the encoding
    // requires it.
    //
    // Correct for `int` for the same reason `ADD EAX, ECX` is: a 32-bit `LEA`
    // computes the effective address in 64 bits, truncates to 32 and
    // zero-extends into the destination, and address arithmetic is congruent
    // mod 2^32 — so the low half is Java's wrapping result whatever the
    // operands' widths. The high half it leaves is the *same* high half the
    // majority of `ir_lower`'s `Int` arms already leave.
    Pattern {
        name: "lea_r32_m",
        emitter: "x64/arith.rs emit_imul_const `[0x8D, 0x04, 0x40|0x80|0xC0]`",
        op: Op::Lea,
        ty: Ty::I32,
        src: OpKind::Mem,
        enc: Enc {
            opcode: Opcode::One(0x8D),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        // LEA reads no memory: it computes the effective address only. Two
        // bytes, not the 64-bit row's three: no REX. That is the *floor* for a
        // base-only operand; `MInst::cost` adds the address's own extra bytes,
        // so the anchored `LEA EAX, [RAX+RAX*n]` still prices at 3.
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "lea_r64_m_disp32",
        emitter: "emit_lea_r64_mem_disp32",
        op: Op::Lea,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x8D),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    // ── Integer ALU ──────────────────────────────────────────────────────
    Pattern {
        name: "add_r64_imm8",
        emitter: "emit_add_r64_imm8 / emit_add_rsp_imm (imm8 branch)",
        op: Op::Add,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(0),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "add_r64_imm32",
        emitter: "emit_add_rsp_imm (imm32 branch)",
        op: Op::Add,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(0),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r64_imm8",
        emitter: "emit_sub_rsp_imm (imm8 branch)",
        op: Op::Sub,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r64_imm32",
        emitter: "emit_sub_rsp_imm (imm32 branch)",
        op: Op::Sub,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "and_r64_imm8",
        emitter: "emit_and_r64_imm8",
        op: Op::And,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r64_imm8",
        emitter: "emit_or_r64_imm8",
        op: Op::Or,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(1),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r64_r64",
        emitter: "emit_sub_r64_r64",
        op: Op::Sub,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x29),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        ..Pattern::BASE
    },
    // ── Register-to-register ALU (the `ir_lower` binary-op family) ───────
    //
    // Every row below reproduces one of the byte literals `ir_lower.rs`'s
    // `lower_data_node` emits for a two-operand integer node. They follow
    // `sub_r64_r64`'s shape exactly — the `xx /r` "store" direction, with the
    // destination in the ModRM `r/m` field — because that is the direction
    // those literals encode (`C8` is `mod=11, reg=RCX, r/m=RAX`, i.e.
    // `OP RAX, RCX`).
    Pattern {
        name: "add_r64_r64",
        emitter: "ir_lower.rs inline `[0x48, 0x01, 0xC8]` (Op::Add, Long/Ref)",
        op: Op::Add,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x01),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        ..Pattern::BASE
    },
    // ── 32-bit ALU immediates ────────────────────────────────────────
    //
    // `Rule::AluImm` fired ZERO times on 850 real Spring Boot compiles before
    // these rows existed — every attempt came back `Unencodable`, because the
    // table had only 64-bit immediate forms and Java arithmetic is 32-bit.
    // That measurement (`docs/feature-designs/jit-machine-level-and-instruction-selection.md`)
    // is what reordered the lane to put these first.
    //
    // Each row reproduces a byte literal `x64.rs`'s constant-folding fast path
    // ALREADY emits, so the table's one trustworthy property — every row
    // anchored to hand-written code it matches byte-for-byte — is preserved.
    // `ir_lower` does not emit these: it materialises the constant into ECX and
    // uses the register form, which is exactly the round trip these rows drop.
    //
    // No REX. The anchors all target EAX (`rex_w: false`, `RexMode::IfNeeded`
    // leaves the prefix off for registers 0-7), which is what makes them
    // byte-identical to the literals below rather than merely equivalent.
    Pattern {
        name: "add_r32_imm8",
        emitter: "x64.rs iadd-const fast path `[0x83, 0xC0, imm8]`",
        op: Op::Add,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(0),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "add_r32_imm32",
        emitter: "x64.rs iadd-const fast path `[0x81, 0xC0] + imm32`",
        op: Op::Add,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(0),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        flags: FlagEffect::W,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r32_imm8",
        emitter: "x64.rs isub-const fast path `[0x83, 0xE8, imm8]`",
        op: Op::Sub,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r32_imm32",
        emitter: "x64.rs isub-const fast path `[0x81, 0xE8] + imm32`",
        op: Op::Sub,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        flags: FlagEffect::W,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "and_r32_imm8",
        emitter: "x64.rs iand-const fast path `[0x83, 0xE0, imm8]`",
        op: Op::And,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r32_imm8",
        emitter: "x64.rs ior-const fast path `[0x83, 0xC8, imm8]`",
        op: Op::Or,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(1),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r32_imm8",
        emitter: "x64.rs ixor-const fast path `[0x83, 0xF0, imm8]`",
        op: Op::Xor,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(6),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_r32_imm8",
        emitter: "x64.rs if_icmp-const fast path `[0x83, 0xF8, imm8]`",
        op: Op::Cmp,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    // The wide sibling of `cmp_r32_imm8`. Without it a `CMP r32, imm32` had no
    // row at all: `select` answered `Err(Constraint(ImmFitsI8))` and
    // `MInst::CmpRI { ty: I32, form: Imm32 }` mapped to no name, so
    // `require_encodable` DISCARDED every compare against a constant outside
    // `-128..=127` — `if (x > 1000)` is the shape, and it is not rare.
    Pattern {
        name: "cmp_r32_imm32",
        emitter: "x64/simd.rs emit_strip_clamp `[0x41, 0x81, 0xF8] + imm32`",
        op: Op::Cmp,
        ty: Ty::I32,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "add_r32_r32",
        emitter: "ir_lower.rs inline `[0x01, 0xC8]` (Op::Add, Int)",
        op: Op::Add,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x01),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sub_r32_r32",
        emitter: "ir_lower.rs inline `[0x29, 0xC8]` (Op::Sub, Int)",
        op: Op::Sub,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x29),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    // The `Int` bitwise forms. `ir_lower` emits these (not the REX.W rows
    // below) for an `IrType::Int` node, to keep the int home word the
    // zero-extension of its low half; see `alu_op_of`.
    Pattern {
        name: "and_r32_r32",
        emitter: "ir_lower.rs inline `[0x21, 0xC8]` (Op::And, Int)",
        op: Op::And,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x21),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r32_r32",
        emitter: "ir_lower.rs inline `[0x09, 0xC8]` (Op::Or, Int)",
        op: Op::Or,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x09),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r32_r32",
        emitter: "ir_lower.rs inline `[0x31, 0xC8]` (Op::Xor, Int)",
        op: Op::Xor,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x31),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "and_r64_r64",
        emitter: "ir_lower.rs inline `[0x48, 0x21, 0xC8]` (Op::And, Long)",
        op: Op::And,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x21),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        ..Pattern::BASE
    },
    Pattern {
        name: "or_r64_r64",
        emitter: "ir_lower.rs inline `[0x48, 0x09, 0xC8]` (Op::Or)",
        op: Op::Or,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x09),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        ..Pattern::BASE
    },
    Pattern {
        name: "xor_r64_r64",
        emitter: "ir_lower.rs inline `[0x48, 0x31, 0xC8]` (Op::Xor)",
        op: Op::Xor,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x31),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r64_r64",
        emitter: "ir_lower.rs inline `[0x48, 0x0F, 0xAF, 0xC1]` (Op::Mul, Long)",
        op: Op::Imul,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0xAF),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 3),
        ..Pattern::BASE
    },
    Pattern {
        name: "imul_r32_r32",
        emitter: "x64.rs inline `[0x0F, 0xAF, 0xC1]` (imul arith step)",
        op: Op::Imul,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::Two(0xAF),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 3),
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_r64_r64",
        emitter: "emit_cmp_r64_r64",
        op: Op::Cmp,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x39),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_r32_r32",
        emitter: "emit_cmp_r32_r32",
        op: Op::Cmp,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x39),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    // ── 64-bit compare against a constant ────────────────────────────────
    //
    // `Op::Cmp` at `Ty::I64` had NO immediate row of either width, so
    // `MInst::CmpRI { ty: Ty::I64, .. }` named nothing and every `long` or
    // reference comparison against a non-zero constant was discarded by
    // `require_encodable` — which is also what pinned `CmpRI`/`I64`/`Imm8` in
    // `the_rows_the_selector_still_lacks_are_exactly_these`. Both widths are
    // anchored to byte literals this backend already emits.
    Pattern {
        name: "cmp_r64_imm8",
        emitter: "x64/arith.rs emit_fp_to_int_nan_fixup `[0x48, 0x83, 0xF8, 0x01]`",
        op: Op::Cmp,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x83),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm8,
        constraints: &[Constraint::ImmFitsI8],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_r64_imm32",
        emitter: "x64/objects.rs ZGC TLAB guard `[0x49, 0x81, 0xF8] + imm32`",
        op: Op::Cmp,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_r64_m_disp32",
        emitter: "emit_cmp_r64_mem_disp32",
        op: Op::Cmp,
        src: OpKind::Mem,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x3B),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(7, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "test_r64_r64",
        emitter: "emit_test_r64_r64",
        op: Op::Test,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x85),
            reg: RegF::Dst,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        constraints: &[Constraint::SameRegister],
        flags: FlagEffect::W,
        ..Pattern::BASE
    },
    Pattern {
        name: "test_r32_r32",
        emitter: "emit_test_r32_r32",
        op: Op::Test,
        ty: Ty::I32,
        enc: Enc {
            opcode: Opcode::One(0x85),
            reg: RegF::Dst,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        constraints: &[Constraint::SameRegister],
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "test_r64_imm32",
        emitter: "emit_test_r64_imm32",
        op: Op::Test,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xF7),
            reg: RegF::Ext(0),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        flags: FlagEffect::W,
        cost: Cost::new(7, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "test_m8_imm8",
        emitter: "emit_test_mem8_imm8",
        op: Op::Test,
        ty: Ty::I8,
        dst: OpKind::Mem,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0xF6),
            reg: RegF::Ext(0),
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::AtLeast8,
        imm: ImmForm::ImmU8,
        constraints: &[
            Constraint::ImmFitsU8,
            Constraint::NoSibBase,
            Constraint::NoIndex,
        ],
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(4, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "test_m8_disp32_imm8",
        emitter: "emit_test_mem8_disp32_imm8",
        op: Op::Test,
        ty: Ty::I8,
        dst: OpKind::Mem,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0xF6),
            reg: RegF::Ext(0),
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ImmFitsU8, Constraint::NoIndex],
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(7, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "cmp_m32_disp32_imm32",
        emitter: "emit_cmp_mem32_disp32_imm32",
        op: Op::Cmp,
        ty: Ty::I32,
        dst: OpKind::Mem,
        src: OpKind::Imm,
        enc: Enc {
            opcode: Opcode::One(0x81),
            reg: RegF::Ext(7),
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Force32,
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32, Constraint::NoIndex],
        flags: FlagEffect::W,
        mem: MemEffect::Read,
        cost: Cost::new(10, 1, 4),
        ..Pattern::BASE
    },
    // ── Shifts ───────────────────────────────────────────────────────────
    Pattern {
        name: "shr_r64_imm8",
        emitter: "emit_shr_r64_imm8",
        op: Op::Shr,
        src: OpKind::Imm,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xC1),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        imm: ImmForm::ImmU8,
        constraints: &[Constraint::ShiftCountFits64],
        flags: FlagEffect::W,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "shl_r32_cl",
        emitter: "x64.rs inline `[0xD3, 0xE0]` (ishl)",
        op: Op::Shl,
        ty: Ty::I32,
        src: OpKind::None,
        enc: Enc {
            opcode: Opcode::One(0xD3),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        fixed: FIXED_SHIFT_CL,
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "shr_r32_cl",
        emitter: "x64.rs inline `[0xD3, 0xE8]` (iushr)",
        op: Op::Shr,
        ty: Ty::I32,
        src: OpKind::None,
        enc: Enc {
            opcode: Opcode::One(0xD3),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        fixed: FIXED_SHIFT_CL,
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sar_r32_cl",
        emitter: "x64.rs inline `[0xD3, 0xF8]` (ishr)",
        op: Op::Sar,
        ty: Ty::I32,
        src: OpKind::None,
        enc: Enc {
            opcode: Opcode::One(0xD3),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        fixed: FIXED_SHIFT_CL,
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "shl_r64_cl",
        emitter: "ir_lower variable shift",
        op: Op::Shl,
        src: OpKind::None,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xD3),
            reg: RegF::Ext(4),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        fixed: FIXED_SHIFT_CL,
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "shr_r64_cl",
        emitter: "ir_lower variable shift",
        op: Op::Shr,
        src: OpKind::None,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xD3),
            reg: RegF::Ext(5),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        fixed: FIXED_SHIFT_CL,
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sar_r64_cl",
        emitter: "ir_lower variable shift",
        op: Op::Sar,
        src: OpKind::None,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xD3),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        fixed: FIXED_SHIFT_CL,
        flags: FlagEffect::W,
        cost: Cost::new(3, 1, 1),
        ..Pattern::BASE
    },
    // ── Division ─────────────────────────────────────────────────────────
    Pattern {
        name: "idiv_r64",
        emitter: "x64.rs inline `[0x48, 0xF7, 0xF9]` (emit_safe_idiv, 64-bit)",
        op: Op::Idiv,
        src: OpKind::None,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0xF7),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        clobbers: CLOBBERS_RAX_RDX,
        fixed: FIXED_DIVIDEND,
        flags: FlagEffect::W,
        cost: Cost::new(3, 10, 40),
        ..Pattern::BASE
    },
    Pattern {
        name: "idiv_r32",
        emitter: "x64.rs inline `[0xF7, 0xF9]` (emit_safe_idiv, 32-bit)",
        op: Op::Idiv,
        ty: Ty::I32,
        src: OpKind::None,
        enc: Enc {
            opcode: Opcode::One(0xF7),
            reg: RegF::Ext(7),
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        clobbers: CLOBBERS_RAX_RDX,
        fixed: FIXED_DIVIDEND,
        flags: FlagEffect::W,
        cost: Cost::new(2, 10, 26),
        ..Pattern::BASE
    },
    Pattern {
        name: "cqo",
        emitter: "x64.rs inline `[0x48, 0x99]` (emit_safe_idiv, 64-bit)",
        op: Op::SignExtendAcc,
        dst: OpKind::None,
        src: OpKind::None,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::One(0x99),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        clobbers: CLOBBERS_RDX,
        fixed: FIXED_ACC,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "cdq",
        emitter: "x64.rs inline `0x99` (emit_safe_idiv, 32-bit)",
        op: Op::SignExtendAcc,
        ty: Ty::I32,
        dst: OpKind::None,
        src: OpKind::None,
        enc: Enc {
            rex: RexMode::Never,
            opcode: Opcode::One(0x99),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        clobbers: CLOBBERS_RDX,
        fixed: FIXED_ACC,
        cost: Cost::new(1, 1, 1),
        ..Pattern::BASE
    },
    // ── Conditional move ─────────────────────────────────────────────────
    Pattern {
        name: "cmov_r64_r64",
        emitter: "emit_cmov_cc_reg_reg",
        op: Op::Cmov,
        extra: OpKind::Cc,
        enc: Enc {
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::OperandTwo,
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        constraints: &[Constraint::CcIsCmov],
        flags: FlagEffect::R,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    // ── Parametric 32-bit ALU ────────────────────────────────────────────
    Pattern {
        name: "alu_r32_r32",
        emitter: "emit_alu_r32_r32",
        op: Op::Alu,
        ty: Ty::I32,
        extra: OpKind::OpByte,
        enc: Enc {
            opcode: Opcode::OperandOne,
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        flags: FlagEffect::W,
        cost: Cost::new(2, 1, 1),
        ..Pattern::BASE
    },
    // ── Stack and control flow ───────────────────────────────────────────
    Pattern {
        name: "push_r64",
        emitter: "emit_push_rbp",
        op: Op::Push,
        src: OpKind::None,
        enc: Enc {
            opcode: Opcode::PlusReg(0x50),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        clobbers: CLOBBERS_RSP,
        mem: MemEffect::Write,
        cost: Cost::new(1, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "pop_r64",
        emitter: "emit_pop_rbp",
        op: Op::Pop,
        src: OpKind::None,
        enc: Enc {
            opcode: Opcode::PlusReg(0x58),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        clobbers: CLOBBERS_RSP,
        mem: MemEffect::Read,
        cost: Cost::new(1, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "ret",
        emitter: "emit_ret",
        op: Op::Ret,
        ty: Ty::Void,
        dst: OpKind::None,
        src: OpKind::None,
        enc: Enc {
            rex: RexMode::Never,
            opcode: Opcode::One(0xC3),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        clobbers: CLOBBERS_RSP,
        mem: MemEffect::Read,
        cost: Cost::new(1, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "jmp_rel32",
        emitter: "emit_jmp_rel32_patch",
        op: Op::Jmp,
        ty: Ty::Void,
        dst: OpKind::Rel32,
        src: OpKind::None,
        enc: Enc {
            rex: RexMode::Never,
            opcode: Opcode::One(0xE9),
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32],
        cost: Cost::new(5, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "jcc_rel32",
        emitter: "emit_jcc_rel32_patch",
        op: Op::Jcc,
        ty: Ty::Void,
        dst: OpKind::Rel32,
        src: OpKind::None,
        extra: OpKind::Cc,
        enc: Enc {
            rex: RexMode::Never,
            opcode: Opcode::OperandTwo,
            modrm: false,
            reg: RegF::Ext(0),
            rm: RmF::None,
            ..Enc::BASE
        },
        imm: ImmForm::Imm32,
        constraints: &[Constraint::ImmFitsI32, Constraint::CcIsJcc],
        flags: FlagEffect::R,
        cost: Cost::new(6, 1, 1),
        ..Pattern::BASE
    },
    // ── XMM ──────────────────────────────────────────────────────────────
    Pattern {
        name: "movq_xmm_r64",
        emitter: "emit_movq_xmm_from_gpr",
        op: Op::Movq,
        dst: OpKind::Xmm,
        src: OpKind::Gpr,
        enc: Enc {
            prefix: Some(0x66),
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0x6E),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        cost: Cost::new(5, 1, 2),
        ..Pattern::BASE
    },
    Pattern {
        name: "movq_r64_xmm",
        emitter: "emit_movq_gpr_from_xmm",
        op: Op::Movq,
        dst: OpKind::Gpr,
        src: OpKind::Xmm,
        enc: Enc {
            prefix: Some(0x66),
            rex_w: true,
            rex: RexMode::Always,
            opcode: Opcode::Two(0x7E),
            reg: RegF::Src,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        cost: Cost::new(5, 1, 2),
        ..Pattern::BASE
    },
    Pattern {
        name: "movq_m_xmm",
        emitter: "emit_movq_mem_rbp_from_xmm",
        op: Op::Movq,
        dst: OpKind::Mem,
        src: OpKind::Xmm,
        enc: Enc {
            prefix: Some(0x66),
            opcode: Opcode::Two(0xD6),
            reg: RegF::Src,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Write,
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "movq_xmm_m",
        emitter: "emit_movq_xmm_from_mem_rbp",
        op: Op::Movq,
        dst: OpKind::Xmm,
        src: OpKind::Mem,
        enc: Enc {
            prefix: Some(0xF3),
            opcode: Opcode::Two(0x7E),
            reg: RegF::Dst,
            rm: RmF::Mem,
            ..Enc::BASE
        },
        disp: DispPolicy::Smallest,
        mem: MemEffect::Read,
        cost: Cost::new(4, 1, 4),
        ..Pattern::BASE
    },
    Pattern {
        name: "movsd_xmm_xmm",
        emitter: "emit_movsd_xmm_xmm",
        op: Op::Movsd,
        ty: Ty::F64,
        dst: OpKind::Xmm,
        src: OpKind::Xmm,
        enc: Enc {
            prefix: Some(0xF2),
            opcode: Opcode::Two(0x10),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "movss_xmm_xmm",
        emitter: "emit_movss_xmm_xmm",
        op: Op::Movss,
        ty: Ty::F32,
        dst: OpKind::Xmm,
        src: OpKind::Xmm,
        enc: Enc {
            prefix: Some(0xF3),
            opcode: Opcode::Two(0x10),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "pxor_xmm_self",
        emitter: "emit_pxor_xmm_self",
        op: Op::Pxor,
        dst: OpKind::Xmm,
        src: OpKind::None,
        enc: Enc {
            prefix: Some(0x66),
            opcode: Opcode::Two(0xEF),
            reg: RegF::Dst,
            rm: RmF::RegDst,
            ..Enc::BASE
        },
        cost: Cost::new(4, 1, 1),
        ..Pattern::BASE
    },
    Pattern {
        name: "sqrtsd_xmm_xmm",
        emitter: "emit_sqrtsd_xmm0",
        op: Op::Sqrtsd,
        ty: Ty::F64,
        dst: OpKind::Xmm,
        src: OpKind::Xmm,
        enc: Enc {
            prefix: Some(0xF2),
            opcode: Opcode::Two(0x51),
            reg: RegF::Dst,
            rm: RmF::RegSrc,
            ..Enc::BASE
        },
        cost: Cost::new(4, 1, 15),
        ..Pattern::BASE
    },
];

// ---------------------------------------------------------------------------
// The matcher
// ---------------------------------------------------------------------------

/// A chosen pattern and the bytes it produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub pattern: &'static Pattern,
    pub encoded: Encoded,
}

/// Encode an integer operation using its allocated result and operand locations.
/// The caller supplies live locations, valid memory addresses and state
/// publication. The sequence writes the destination and arithmetic flags.
/// A destructive overlap that needs an additional temporary declines the form.
pub fn encode_allocated_alu(
    op: Op,
    ty: Ty,
    dst: u8,
    mut lhs: u8,
    mut rhs: Operand,
) -> Option<Vec<u8>> {
    if !matches!(ty, Ty::I32 | Ty::I64)
        || !matches!(
            op,
            Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor | Op::Imul | Op::Shl | Op::Shr | Op::Sar
        )
        || dst > 15
        || lhs > 15
    {
        return None;
    }
    if op == Op::Imul {
        if let Operand::Imm(imm) = rhs {
            return select(
                &Req::new(op, ty, Operand::Gpr(dst), Operand::Gpr(lhs))
                    .with_extra(Operand::Imm(imm)),
            )
            .ok()
            .map(|instruction| instruction.encoded.bytes);
        }
    }
    if matches!(op, Op::Shl | Op::Shr | Op::Sar) {
        let Operand::Imm(count) = rhs else {
            return None;
        };
        // Java masks the shift count before applying the machine operation.
        rhs = Operand::Imm(count & if ty == Ty::I64 { 63 } else { 31 });
    }
    if dst != lhs {
        match rhs {
            Operand::Gpr(reg) if reg == dst => {
                if op == Op::Sub {
                    return None;
                }
                rhs = Operand::Gpr(lhs);
                lhs = dst;
            }
            Operand::Mem(mem)
                if mem.base == dst || mem.index.is_some_and(|index| index.reg == dst) =>
            {
                return None;
            }
            _ => {}
        }
    }
    let mut bytes = Vec::with_capacity(16);
    if dst != lhs {
        let copy = select(&Req::new(Op::Mov, ty, Operand::Gpr(dst), Operand::Gpr(lhs))).ok()?;
        bytes.extend_from_slice(&copy.encoded.bytes);
    }
    let instruction = select(&Req::new(op, ty, Operand::Gpr(dst), rhs)).ok()?;
    bytes.extend_from_slice(&instruction.encoded.bytes);
    Some(bytes)
}

/// Look a pattern up by name.
pub fn pattern(name: &str) -> Option<&'static Pattern> {
    PATTERNS.iter().find(|p| p.name == name)
}

/// Encode a named pattern directly, bypassing the matcher.
///
/// Use for the [`Mode::Explicit`] rows and wherever a call site must pin one
/// specific encoding (an inline-cache site whose immediate is rewritten in
/// place, for instance).
pub fn encode_named(name: &str, a: &Args) -> Result<Encoded, SelError> {
    match pattern(name) {
        Some(p) => p.encode(a),
        None => Err(SelError::UnknownPattern),
    }
}

/// Select and encode the best pattern for `req`.
///
/// Candidates are the [`Mode::Auto`] rows whose operation, type and operand
/// kinds match, minus those whose constraints the operands violate. Among the
/// survivors the shortest encoding wins; ties break on `uops`, then
/// `latency`, then table order.
///
/// The three-row `MOV r64, imm` family is what makes this worth doing: the
/// selector reproduces `emit_mov_imm64`'s zero → `XOR`, i32 → `C7`,
/// otherwise → `B8+rd` shrink chain without any of those decisions being
/// written down as control flow.
///
/// The ranking never looks at [`Pattern::flags`]: the `XOR` zeroing row
/// writes EFLAGS. A caller whose flags are live must ask with
/// [`Req::preserving_flags`], which removes every flag-writing row from the
/// candidates.
pub fn select(req: &Req) -> Result<Selection, SelError> {
    let args = req.args();
    let mut best: Option<Selection> = None;
    let mut last_err: Option<SelError> = None;
    for p in PATTERNS {
        if p.mode != Mode::Auto {
            continue;
        }
        if p.op != req.op || p.ty != req.ty {
            continue;
        }
        if req.preserve_flags && p.flags.writes {
            continue;
        }
        if p.dst != req.dst.kind() || p.src != req.src.kind() || p.extra != req.extra.kind() {
            continue;
        }
        // A forced-width displacement and a smallest-form displacement are
        // different requests, not two encodings of one request: only the
        // matching row may answer.
        if p.is_memory() {
            let wants_force32 = args.mem.force_disp32;
            let row_force32 = matches!(p.disp, DispPolicy::Force32);
            if wants_force32 != row_force32 {
                continue;
            }
        }
        match p.encode(&args) {
            Ok(e) => {
                let better = match &best {
                    None => true,
                    Some(b) => {
                        (e.bytes.len(), p.cost.uops, p.cost.latency)
                            < (
                                b.encoded.bytes.len(),
                                b.pattern.cost.uops,
                                b.pattern.cost.latency,
                            )
                    }
                };
                if better {
                    best = Some(Selection {
                        pattern: p,
                        encoded: e,
                    });
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    match best {
        Some(s) => Ok(s),
        None => Err(last_err.unwrap_or(SelError::NoPattern {
            op: req.op,
            ty: req.ty,
        })),
    }
}

// ---------------------------------------------------------------------------
// Structural decoder (round-trip disassembly)
// ---------------------------------------------------------------------------

/// An instruction taken apart again.
///
/// The decoder derives everything from the byte stream except one bit of
/// table input — whether the opcode carries a ModRM byte — which x86 does not
/// encode structurally. Everything else (prefixes, REX, one- versus two-byte
/// opcode, SIB presence, displacement width) follows from the bytes, so
/// decoding an encoding and recovering the operands it was built from is a
/// genuine round trip and not a restatement of [`Pattern::encode`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    pub prefixes: Vec<u8>,
    pub rex: Option<u8>,
    /// One byte, or `0F` followed by the second byte.
    pub opcode: Vec<u8>,
    pub modrm: Option<u8>,
    pub sib: Option<u8>,
    /// The decoded displacement (zero when the operand has none).
    pub disp: i64,
    pub disp_len: usize,
    /// Everything after the displacement.
    pub imm: Vec<u8>,
    pub len: usize,
}

impl Decoded {
    fn rex_bit(&self, mask: u8) -> u8 {
        match self.rex {
            Some(r) if r & mask != 0 => 1,
            _ => 0,
        }
    }

    /// REX.W.
    pub fn wide(&self) -> bool {
        self.rex_bit(0x08) == 1
    }

    /// The ModRM `mod` field.
    pub fn mod_bits(&self) -> Option<u8> {
        self.modrm.map(|m| m >> 6)
    }

    /// The full 0..=15 register number in the ModRM `reg` field.
    pub fn reg(&self) -> Option<u8> {
        self.modrm
            .map(|m| (self.rex_bit(0x04) << 3) | ((m >> 3) & 7))
    }

    /// The full 0..=15 register number in the ModRM `r/m` field, for
    /// register-direct operands.
    pub fn rm_reg(&self) -> Option<u8> {
        self.modrm.map(|m| (self.rex_bit(0x01) << 3) | (m & 7))
    }

    /// The base register of a memory operand, taking the SIB byte into
    /// account.
    pub fn base(&self) -> Option<u8> {
        let m = self.modrm?;
        let low = match self.sib {
            Some(s) => s & 7,
            None => m & 7,
        };
        Some((self.rex_bit(0x01) << 3) | low)
    }

    /// The index register of a memory operand, or `None` for the "no index"
    /// SIB encoding.
    pub fn index(&self) -> Option<u8> {
        let s = self.sib?;
        let idx = (self.rex_bit(0x02) << 3) | ((s >> 3) & 7);
        if idx == RSP {
            None
        } else {
            Some(idx)
        }
    }

    /// The SIB scale factor: 1, 2, 4 or 8.
    pub fn scale(&self) -> Option<u8> {
        self.sib.map(|s| 1u8 << (s >> 6))
    }
}

/// Take an encoded instruction apart.
///
/// Returns `None` when the byte stream is truncated. `has_modrm` must match
/// the pattern's [`Enc::modrm`].
pub fn decode(bytes: &[u8], has_modrm: bool) -> Option<Decoded> {
    let mut d = Decoded {
        len: bytes.len(),
        ..Decoded::default()
    };
    let mut i = 0usize;
    while i < bytes.len() && matches!(bytes[i], 0x66 | 0xF0 | 0xF2 | 0xF3) {
        d.prefixes.push(bytes[i]);
        i += 1;
    }
    if i < bytes.len() && (0x40..=0x4F).contains(&bytes[i]) {
        d.rex = Some(bytes[i]);
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == 0x0F {
        d.opcode.push(0x0F);
        i += 1;
        if i >= bytes.len() {
            return None;
        }
    }
    d.opcode.push(bytes[i]);
    i += 1;
    if has_modrm {
        if i >= bytes.len() {
            return None;
        }
        let m = bytes[i];
        i += 1;
        d.modrm = Some(m);
        let md = m >> 6;
        let rm = m & 7;
        if md != 0b11 && rm == 0b100 {
            if i >= bytes.len() {
                return None;
            }
            d.sib = Some(bytes[i]);
            i += 1;
        }
        let disp_len = match md {
            // `mod=00` carries a disp32 only in the RIP-relative (`r/m=101`)
            // and no-base (SIB `base=101`) forms.
            0b00 => {
                if rm == 0b101 {
                    4
                } else if let Some(s) = d.sib {
                    if s & 7 == 0b101 {
                        4
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            0b01 => 1,
            0b10 => 4,
            _ => 0,
        };
        if i + disp_len > bytes.len() {
            return None;
        }
        d.disp = match disp_len {
            // Cast: the displacement byte is read back as SIGNED, which is
            // the whole reason `disp.rs` exists.
            1 => bytes[i] as i8 as i64,
            4 => i32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as i64,
            _ => 0,
        };
        d.disp_len = disp_len;
        i += disp_len;
    }
    d.imm = bytes[i..].to_vec();
    Some(d)
}

fn reg_name(kind: OpKind, reg: u8) -> &'static str {
    let idx = usize::from(reg & 15);
    match kind {
        OpKind::Xmm => XMM_NAMES[idx],
        _ => GPR64_NAMES[idx],
    }
}

fn mem_text(base: u8, index: Option<u8>, scale: u8, disp: i64) -> String {
    let mut s = format!("[{}", GPR64_NAMES[usize::from(base & 15)]);
    if let Some(ix) = index {
        s.push_str(&format!(" + {}*{scale}", GPR64_NAMES[usize::from(ix & 15)]));
    }
    if disp < 0 {
        s.push_str(&format!(" - {}", disp.unsigned_abs()));
    } else if disp > 0 {
        s.push_str(&format!(" + {disp}"));
    }
    s.push(']');
    s
}

/// Render a decoded instruction as text.
///
/// Operands are listed in **ModRM field order** — the `reg` field first, then
/// the `r/m` field — not in Intel destination-first order, because half the
/// x86 opcodes swap those roles and a rendering that quietly reorders them
/// would hide exactly the mistake this is here to catch.
///
/// The pattern supplies one thing the byte stream cannot: whether a register
/// field names a GPR or an XMM register. Everything else comes from `d`.
pub fn render(p: &Pattern, d: &Decoded) -> String {
    let mut ops: Vec<String> = Vec::new();
    if p.enc.modrm {
        match p.enc.reg {
            RegF::Dst => ops.push(reg_name(p.dst, d.reg().unwrap_or(0)).to_string()),
            RegF::Src => ops.push(reg_name(p.src, d.reg().unwrap_or(0)).to_string()),
            RegF::Ext(n) => ops.push(format!("/{n}")),
        }
        match p.enc.rm {
            RmF::RegDst => ops.push(reg_name(p.dst, d.rm_reg().unwrap_or(0)).to_string()),
            RmF::RegSrc => ops.push(reg_name(p.src, d.rm_reg().unwrap_or(0)).to_string()),
            RmF::Mem => ops.push(mem_text(
                d.base().unwrap_or(0),
                d.index(),
                d.scale().unwrap_or(1),
                d.disp,
            )),
            RmF::None => {}
        }
    } else if matches!(p.enc.opcode, Opcode::PlusReg(_)) {
        // The register is folded into the opcode byte; recover it from there.
        let low = d.opcode.last().copied().unwrap_or(0) & 7;
        let full = ((d.rex.unwrap_or(0) & 0x01) << 3) | low;
        ops.push(reg_name(p.dst, full).to_string());
    }
    if !d.imm.is_empty() {
        ops.push(format!("imm{:02X?}", d.imm));
    }
    format!("{:?}({}) {}", p.op, p.name, ops.join(", "))
}

// ---------------------------------------------------------------------------
// IR-level instruction selection
// ---------------------------------------------------------------------------
//
// Everything above this banner is about *encoding*: given operands already in
// registers, which byte sequence expresses the operation. This section is
// about **selection**: given a region of the sea-of-nodes IR, which machine
// instructions should exist at all.
//
// `ir_lower::lower_data_node` answers that question one node at a time and
// through the frame: every binary node emits `MOV RAX,[slot]; MOV RCX,[slot];
// <op>; MOV [slot],RAX`. That shape leaves five families of x86-64 instruction
// on the floor, and this section is a pattern matcher that finds them:
//
//   * **address-mode folding** — `a + (i << 2) + 16` is one `LEA`, not three
//     ALU instructions and three frame round-trips ([`match_address`]);
//   * **compare-and-branch fusion** — an `Op::Cmp` that only feeds an `Op::If`
//     never needs its 0/1 value materialised through `SETcc`/`MOVZX`;
//   * **test-versus-compare-against-zero** — `CMP r, 0` and `TEST r, r` leave
//     CF, OF, SF and ZF identical, and the second is shorter;
//   * **`LEA` for three-operand adds and small multiplies** — `LEA` is
//     non-destructive, so a left-hand side that is still live afterwards does
//     not have to be copied first, and `x*3` / `x*5` / `x*9` are one `LEA`;
//   * **immediate and memory-operand folding** — a constant that fits the
//     immediate field, and a load whose value has exactly one arithmetic
//     consumer, both disappear into that consumer's operand.
//
// # What makes this safe
//
// Two gates, and neither of them is optional.
//
// **The immediate-width gate.** A constant only folds into an immediate field
// when the *checked* conversion succeeds — [`imm_form_for`] is `i8::try_from` /
// `i32::try_from`, never `as i8` / `as i32`. A displacement only folds when
// [`Disp::encode32`] accepts it. Both are the helpers `disp.rs` exists to
// concentrate; this module adds no narrowing of its own.
//
// **The load-folding gate.** Folding a load into a later arithmetic user moves
// the load *down* the block, past everything between them. [`may_fold_load`]
// refuses unless the load has exactly one value consumer, is not named by a
// safepoint snapshot, is a plain (non-volatile, non-safepoint) read, and every
// node it would cross answers [`Reorder::Allowed`] to [`Graph::may_reorder`].
// Get that wrong and the load reads the value of a *later* store — the same
// defect class this branch already found once in escape analysis.
//
// # Fail-closed
//
// There is no silent no-op anywhere in this section. A node no rule matches
// becomes a [`Rule::Generic`] tile naming that node, which is an instruction to
// the caller ("lower this the old way"), not an absence. A tile whose
// instructions the pattern table cannot encode is *discarded* under the default
// [`SelectOptions::require_encodable`], and the reason is recorded in
// [`BlockSelection::notes`]. A block always comes back with every one of its
// nodes covered exactly once — [`BlockSelection::covers`] is the check, and the
// tests assert it.
//
// # Wiring
//
// Two callers, both off by default: `shadow_select_method` (counts only,
// `CRATONVM_JIT_IR_ISEL_SHADOW`) and `ir_lower::build_mir_plan`, whose
// `Rule::AluReg` tiles become emitted bytes under `CRATONVM_JIT=ir-isel-emit`.
// See `docs/jit/instruction-selection.md` for what the rest of the wiring has
// to do and what is still unvalidated.

use crate::ir::{
    is_memory_token_slot, CmpOp, Graph, IrType, NodeId, Op as IrOp, Reorder, ReorderBlock,
};
use crate::ir_schedule::Schedule;

/// Largest number of IR nodes [`match_address`] will walk before giving up.
///
/// An address expression that needs more than this many nodes is not an
/// address expression; the bound keeps a pathological operand chain from
/// turning into a pathological compile.
pub const ADDR_MATCH_BUDGET: usize = 24;

/// The cost of leaving a node to `ir_lower::lower_data_node`.
///
/// Read off that function's actual shape for a binary integer node:
/// `load_to_rax` (a `MOV r64, [RBP+disp8]`, 4 bytes), `load_to_rcx` (4 bytes),
/// the operation itself (2–4 bytes) and `store_rax` (4 bytes) — call it 15
/// bytes and 4 micro-ops, with a latency of one load (≈4 cycles, the two loads
/// issue in parallel) plus the operation plus the store.
///
/// It is deliberately the most expensive thing the cost model can name: a rule
/// that covers *more* IR nodes must win, and this is what makes it win.
pub const GENERIC_COST: Cost = Cost::new(15, 4, 9);

// ── Cost accumulation ────────────────────────────────────────────────────

/// The cost of a *sequence* of instructions.
///
/// Widened from [`Cost`]'s `u8` fields, which are per-row and would saturate
/// after a handful of instructions. Saturating rather than wrapping: an
/// overflowing cost must read as "very expensive", never as "free".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeqCost {
    pub bytes: u32,
    pub uops: u32,
    pub latency: u32,
}

impl SeqCost {
    /// Lift a single row's cost.
    pub fn of(c: Cost) -> SeqCost {
        SeqCost {
            bytes: u32::from(c.bytes),
            uops: u32::from(c.uops),
            latency: u32::from(c.latency),
        }
    }

    /// Sequence composition: bytes and micro-ops add, and so does latency —
    /// the instructions a tile emits for one IR node are a dependence chain
    /// (each consumes the previous one's result), not independent work.
    pub fn then(self, o: SeqCost) -> SeqCost {
        SeqCost {
            bytes: self.bytes.saturating_add(o.bytes),
            uops: self.uops.saturating_add(o.uops),
            latency: self.latency.saturating_add(o.latency),
        }
    }

    /// The ranking key, **micro-ops first**.
    ///
    /// Front-end throughput is what a tiling decision actually buys: the whole
    /// point of folding a load into its consumer or collapsing an add tree into
    /// one `LEA` is to issue fewer micro-ops. Bytes break the tie (instruction
    /// cache), then latency. Ordering matters — ranking by bytes first would
    /// prefer a two-byte `ADD r32, r32` plus a three-byte `MOV` over a
    /// four-byte `LEA` that does both.
    pub fn key(self) -> (u32, u32, u32) {
        (self.uops, self.bytes, self.latency)
    }
}

// ── Addressing modes over IR values ──────────────────────────────────────

/// `base + index*scale + disp`, with IR nodes where [`Mem`] has registers.
///
/// The register allocator turns this into a [`Mem`]; until then the operands
/// are values, so the two x86 rules that are about *register numbers* — an
/// index may not be RSP, and an RSP/R12 base needs a SIB byte — cannot be
/// checked here. They are [`Mem`]'s job and the table already states them
/// ([`Constraint::IndexNotRsp`], [`base_requires_sib`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrAddr {
    /// The unscaled term.
    pub base: Option<NodeId>,
    /// The scaled term.
    pub index: Option<NodeId>,
    /// 1, 2, 4 or 8. Meaningless (and always 1) when there is no index.
    pub scale: u8,
    /// The constant term, in bytes.
    pub disp: i64,
}

impl IrAddr {
    /// An empty address — no terms at all.
    pub fn empty() -> IrAddr {
        IrAddr {
            base: None,
            index: None,
            scale: 1,
            disp: 0,
        }
    }

    /// How many machine terms this address carries. One term is not worth an
    /// `LEA`: `[x]` is a register copy and `[x + 8]` is an `ADD`.
    pub fn terms(&self) -> usize {
        usize::from(self.base.is_some())
            + usize::from(self.index.is_some())
            + usize::from(self.disp != 0)
    }

    /// Is this expressible as an x86-64 memory operand?
    ///
    /// Three questions, each asked of the code that owns it:
    ///
    /// * the scale goes to [`scale_bits`];
    /// * the displacement goes to [`Disp::encode32`] — the checked helper, so a
    ///   value past `i32` is an error and never a truncation;
    /// * a base is **required**. `[index*scale + disp32]` is a legal x86
    ///   operand (`mod=00`, SIB `base=101`) but [`Mem`] has no way to say "no
    ///   base", so admitting one here would produce an [`IrAddr`] that cannot
    ///   be lowered. Refusing is the fail-closed direction: the caller falls
    ///   back to a shift or a multiply.
    pub fn check(&self) -> Result<(), SelError> {
        if self.index.is_some() {
            scale_bits(self.scale)?;
        } else if self.scale != 1 {
            return Err(SelError::BadScale { scale: self.scale });
        }
        if self.base.is_none() {
            return Err(SelError::NoBaseRegister);
        }
        Disp::encode32(self.disp)?;
        Ok(())
    }

    /// Encoded length of the ModRM/SIB/displacement tail this address needs,
    /// for the cost model: one ModRM byte, a SIB byte when there is an index or
    /// an RSP-class base (unknowable here, so assumed absent), and the
    /// displacement bytes [`Disp::encode`] would choose.
    fn operand_bytes(&self) -> u32 {
        let sib = u32::from(self.index.is_some());
        let disp = match Disp::encode(self.disp) {
            Ok(d) => u32::try_from(d.byte_len()).unwrap_or(4),
            // Unencodable: `check` refuses it, so this arm only runs for a
            // cost query on an address nobody will emit. Price it as the
            // widest form rather than as free.
            Err(_) => 4,
        };
        1 + sib + disp
    }
}

/// Why [`match_address`] declined to build an [`IrAddr`].
///
/// Every arm names a node or a value: a refusal that cannot be attributed is a
/// refusal nobody can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrRefusal {
    /// The root is not an arithmetic node an address can be read out of.
    NotAnAddress(NodeId),
    /// A second scaled term appeared; x86 has one index.
    TooManyIndices(NodeId),
    /// A third unscaled term appeared; x86 has one base and one index.
    TooManyTerms(NodeId),
    /// The folded displacement overflowed `i64` before it could even be
    /// range-checked against `i32`.
    DispOverflow(i64),
    /// The walk hit [`ADDR_MATCH_BUDGET`].
    Budget,
    /// The id does not name a node.
    Unknown(NodeId),
    /// The address is well-formed but has no x86-64 memory operand.
    Unencodable(SelError),
}

/// An address expression and the interior nodes it absorbed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddrMatch {
    /// The address itself.
    pub addr: IrAddr,
    /// The interior nodes (root included) whose computation the address
    /// subsumes. Every one of them must be covered by the tile that uses this
    /// match, and none of them may be emitted separately.
    pub absorbed: Vec<NodeId>,
}

// ── Value uses ───────────────────────────────────────────────────────────

/// How many consumers each value has, and which values a deopt frame names.
///
/// **Value** uses, not edge uses. `Graph::use_counts` counts every input slot,
/// including the memory-token edges that `IrBuilder` threads through *loads*
/// (`self.mem = load` after every `getfield`). Counting those would make every
/// load look multiply-used and would refuse every fold — the token edge is an
/// ordering edge, and ordering is what [`may_fold_load`] checks separately.
///
/// `pinned` is the other half of "exactly one use": a value a
/// [`crate::ir::SafepointSnapshot`] names has to *exist* somewhere the deopt
/// writer can point at, so it may not be folded away even when its only
/// in-graph consumer is the folding user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValueUses {
    counts: Vec<u32>,
    pinned: Vec<bool>,
}

impl ValueUses {
    /// Count every value edge in `graph`, once.
    pub fn of(graph: &Graph) -> ValueUses {
        let n = graph.nodes.len();
        let mut counts = vec![0u32; n];
        let mut pinned = vec![false; n];
        for node in &graph.nodes {
            for (i, &inp) in node.inputs.iter().enumerate() {
                if is_memory_token_slot(node, i) {
                    continue;
                }
                if let Some(slot) = counts.get_mut(inp as usize) {
                    *slot = slot.saturating_add(1);
                }
            }
        }
        for sp in &graph.safepoints {
            for &id in sp.locals.iter().chain(sp.stack.iter()).chain(sp.monitors.iter()) {
                if let Some(slot) = pinned.get_mut(id as usize) {
                    *slot = true;
                }
            }
        }
        ValueUses { counts, pinned }
    }

    /// Value consumers of `id`. An unknown id reads as heavily used, so an
    /// out-of-range node is never folded.
    pub fn count(&self, id: NodeId) -> u32 {
        self.counts.get(id as usize).copied().unwrap_or(u32::MAX)
    }

    /// Does a safepoint snapshot name `id`?  Unknown ids read as pinned.
    pub fn is_pinned(&self, id: NodeId) -> bool {
        self.pinned.get(id as usize).copied().unwrap_or(true)
    }

    /// Exactly one value consumer, and no deopt frame needs it.
    pub fn single_use(&self, id: NodeId) -> bool {
        self.count(id) == 1 && !self.is_pinned(id)
    }
}

// ── Selection context ────────────────────────────────────────────────────

/// Everything the tiler needs about one scheduled basic block.
pub struct SelCtx<'g> {
    /// The graph being selected over.
    pub graph: &'g Graph,
    uses: ValueUses,
    checked_array_reads: Option<Vec<Option<IrAddr>>>,
    /// Position of each node in `order`, or `usize::MAX` when it is not in this
    /// block.
    pos: Vec<usize>,
    /// The block's data nodes, in the order `ir_lower` will emit them.
    order: Vec<NodeId>,
}

impl<'g> SelCtx<'g> {
    /// Build a context for `block`, which must be the block's data nodes in
    /// scheduled order (`ir_schedule::Block::nodes`).
    pub fn new(graph: &'g Graph, block: &[NodeId]) -> SelCtx<'g> {
        let mut ctx = SelCtx {
            graph,
            uses: ValueUses::of(graph),
            checked_array_reads: None,
            pos: vec![usize::MAX; graph.nodes.len()],
            order: Vec::new(),
        };
        ctx.set_block(block);
        ctx
    }

    fn set_block(&mut self, block: &[NodeId]) {
        for id in self.order.drain(..) {
            if let Some(slot) = self.pos.get_mut(id as usize) {
                *slot = usize::MAX;
            }
        }
        for (i, &id) in block.iter().enumerate() {
            if let Some(slot) = self.pos.get_mut(id as usize) {
                // Preserve the first position if the input names a node twice.
                if *slot == usize::MAX {
                    *slot = i;
                }
            }
        }
        self.order.extend_from_slice(block);
    }

    /// The value-use table.
    pub fn uses(&self) -> &ValueUses {
        &self.uses
    }

    /// The block's nodes, in scheduled order.
    pub fn order(&self) -> &[NodeId] {
        &self.order
    }

    /// Position of `id` within the block, or `None` when it lives elsewhere.
    pub fn position(&self, id: NodeId) -> Option<usize> {
        match self.pos.get(id as usize).copied() {
            Some(p) if p != usize::MAX => Some(p),
            _ => None,
        }
    }

    /// Is `id` scheduled into this block?
    pub fn in_block(&self, id: NodeId) -> bool {
        self.position(id).is_some()
    }

    /// May `id` be absorbed into a bigger tile?
    ///
    /// It must be in *this* block — absorbing a node the scheduler placed
    /// elsewhere (a loop-invariant expression hoisted out of the loop, say)
    /// would recompute it on every iteration *and* leave the original standing.
    /// And it must have exactly one consumer, or absorbing it deletes a
    /// register something else reads.
    pub fn absorbable(&self, id: NodeId) -> bool {
        self.in_block(id) && self.uses.single_use(id)
    }

    /// The node's IR type mapped onto the selector's operand type, or `None`
    /// for a type no integer rule applies to.
    pub fn int_ty(&self, id: NodeId) -> Option<Ty> {
        match self.graph.node_opt(id)?.ty {
            IrType::Int => Some(Ty::I32),
            IrType::Long | IrType::Ref => Some(Ty::I64),
            _ => None,
        }
    }

    /// The constant `id` names, when it names one.
    pub fn const_of(&self, id: NodeId) -> Option<i64> {
        match self.graph.node_opt(id)?.op {
            IrOp::Const(v) => Some(v),
            _ => None,
        }
    }

    /// Input `idx` of `id`, `None` for a missing edge or an unknown node.
    pub fn input(&self, id: NodeId, idx: usize) -> Option<NodeId> {
        self.graph.node_opt(id)?.input_opt(idx)
    }
}

// ── Address matching ─────────────────────────────────────────────────────

/// Accumulator for [`match_address`]: at most one base, at most one index.
struct AddrAcc {
    addr: IrAddr,
    absorbed: Vec<NodeId>,
}

impl AddrAcc {
    fn new() -> AddrAcc {
        AddrAcc {
            addr: IrAddr::empty(),
            absorbed: Vec::new(),
        }
    }

    /// A scaled term `x * s`, `s` in 2/4/8.
    fn scaled(&mut self, x: NodeId, s: u8) -> Result<(), AddrRefusal> {
        if self.addr.index.is_some() {
            return Err(AddrRefusal::TooManyIndices(x));
        }
        self.addr.index = Some(x);
        self.addr.scale = s;
        Ok(())
    }

    /// An unscaled term. Fills the base first; a second one becomes a
    /// scale-1 index, which is exactly the three-operand `LEA r, [a + b]`.
    fn plain(&mut self, x: NodeId) -> Result<(), AddrRefusal> {
        if self.addr.base.is_none() {
            self.addr.base = Some(x);
            return Ok(());
        }
        if self.addr.index.is_none() {
            self.addr.index = Some(x);
            self.addr.scale = 1;
            return Ok(());
        }
        Err(AddrRefusal::TooManyTerms(x))
    }

    /// A constant term. `checked_add`, so a folded displacement can never wrap
    /// into a small-looking number.
    fn disp(&mut self, c: i64) -> Result<(), AddrRefusal> {
        self.addr.disp = self
            .addr
            .disp
            .checked_add(c)
            .ok_or(AddrRefusal::DispOverflow(c))?;
        Ok(())
    }
}

/// Absorb the shift count / multiplier constant of a folded `Shl` or `Mul`.
///
/// The constant becomes part of the SIB scale field, so its register is gone
/// too — but only when nothing else reads it. Without this the tile would leave
/// a dead `MOV r, 2` behind for every scaled index.
///
/// `a` and `b` are the node's two operands and `value` the one that is *not*
/// the constant; whichever of `a`/`b` is not `value` is the constant.
fn absorb_constant(ctx: &SelCtx, acc: &mut AddrAcc, a: NodeId, b: NodeId, value: NodeId) {
    let k = if a == value { b } else { a };
    if k != value && ctx.absorbable(k) && ctx.const_of(k).is_some() {
        acc.absorbed.push(k);
    }
}

/// Read a `base + index*scale + disp` address out of the pure integer
/// arithmetic rooted at `root`.
///
/// The grammar, applied to the root and then recursively to every term whose
/// node [`SelCtx::absorbable`] accepts:
///
/// ```text
///   term := Add(term, term)          -- split
///         | Shl(x, Const k), k<=3    -- index x, scale 1<<k
///         | Mul(x, Const c)          -- c in 1/2/4/8 -> index x, scale c
///         | Const(c)                 -- disp += c   (never at the root)
///         | anything else            -- base, or a scale-1 index
/// ```
///
/// The root additionally admits the *small multiply* forms `x*2`, `x*3`, `x*5`
/// and `x*9`, which become `[x + x*1]`, `[x + x*2]`, `[x + x*4]` and
/// `[x + x*8]`. `x*4` and `x*8` are deliberately **not** admitted: they need a
/// base-less operand, which [`IrAddr::check`] refuses (see there), and a shift
/// covers them anyway.
///
/// The `absorbable` gate is what makes this a *selection* rather than a
/// rewrite: an interior node with a second consumer stays where it is and
/// becomes the address's base, so nothing is ever computed twice.
pub fn match_address(ctx: &SelCtx, root: NodeId) -> Result<AddrMatch, AddrRefusal> {
    let rootn = ctx.graph.node_opt(root).ok_or(AddrRefusal::Unknown(root))?;
    if !matches!(rootn.op, IrOp::Add | IrOp::Shl | IrOp::Mul) {
        return Err(AddrRefusal::NotAnAddress(root));
    }

    let mut acc = AddrAcc::new();
    let mut work: Vec<NodeId> = vec![root];
    let mut steps = 0usize;

    while let Some(id) = work.pop() {
        steps += 1;
        if steps > ADDR_MATCH_BUDGET {
            return Err(AddrRefusal::Budget);
        }
        let is_root = id == root;
        let node = ctx.graph.node_opt(id).ok_or(AddrRefusal::Unknown(id))?;
        // The root is being *replaced* by the address, so it absorbs itself.
        // Every other node has to earn it.
        let may_absorb = is_root || ctx.absorbable(id);

        // `x << k` and `x * c` at the root, where the whole node becomes the
        // address. Handled before the generic arms so a root multiply can use
        // the two-term small-multiply forms.
        if is_root && matches!(node.op, IrOp::Mul) {
            let x = node.input_opt(0).ok_or(AddrRefusal::Unknown(id))?;
            let k = node.input_opt(1).ok_or(AddrRefusal::Unknown(id))?;
            // Whichever side is the constant is the multiplier; the other is
            // the value that becomes both the base and the index.
            let (val, other) = match (ctx.const_of(x), ctx.const_of(k)) {
                (_, Some(c)) => (c, x),
                (Some(c), _) => (c, k),
                _ => return Err(AddrRefusal::NotAnAddress(root)),
            };
            acc.absorbed.push(root);
            absorb_constant(ctx, &mut acc, x, k, other);
            // `x*4` and `x*8` are missing on purpose: they need `[x*4]` with
            // no base, which `IrAddr::check` refuses. A shift covers them.
            match val {
                1 => acc.plain(other)?,
                2 => {
                    acc.plain(other)?;
                    acc.plain(other)?;
                }
                3 => {
                    acc.plain(other)?;
                    acc.scaled(other, 2)?;
                }
                5 => {
                    acc.plain(other)?;
                    acc.scaled(other, 4)?;
                }
                9 => {
                    acc.plain(other)?;
                    acc.scaled(other, 8)?;
                }
                _ => return Err(AddrRefusal::NotAnAddress(root)),
            }
            continue;
        }
        if is_root && matches!(node.op, IrOp::Shl) {
            let x = node.input_opt(0).ok_or(AddrRefusal::Unknown(id))?;
            let k = node.input_opt(1).ok_or(AddrRefusal::Unknown(id))?;
            // Only `x << 1` has a base-ful form (`[x + x]`); `<< 2` and `<< 3`
            // would need a base-less operand.
            if ctx.const_of(k) == Some(1) {
                acc.absorbed.push(root);
                absorb_constant(ctx, &mut acc, x, k, x);
                acc.plain(x)?;
                acc.plain(x)?;
                continue;
            }
            return Err(AddrRefusal::NotAnAddress(root));
        }

        match &node.op {
            IrOp::Add if may_absorb => {
                let a = node.input_opt(0).ok_or(AddrRefusal::Unknown(id))?;
                let b = node.input_opt(1).ok_or(AddrRefusal::Unknown(id))?;
                acc.absorbed.push(id);
                // Push the right operand first so the left is popped first and
                // becomes the base — a deterministic, reproducible choice.
                work.push(b);
                work.push(a);
            }
            IrOp::Shl if may_absorb && !is_root => match (node.input_opt(0), node.input_opt(1)) {
                (Some(x), Some(k)) => match ctx.const_of(k) {
                    Some(sh @ (0 | 1 | 2 | 3)) => {
                        acc.absorbed.push(id);
                        absorb_constant(ctx, &mut acc, x, k, x);
                        match sh {
                            0 => acc.plain(x)?,
                            1 => acc.scaled(x, 2)?,
                            2 => acc.scaled(x, 4)?,
                            _ => acc.scaled(x, 8)?,
                        }
                    }
                    _ => acc.plain(id)?,
                },
                _ => acc.plain(id)?,
            },
            IrOp::Mul if may_absorb && !is_root => {
                let folded = match (node.input_opt(0), node.input_opt(1)) {
                    (Some(x), Some(y)) => match (ctx.const_of(y), ctx.const_of(x)) {
                        (Some(c), _) => Some((x, y, c)),
                        (_, Some(c)) => Some((y, x, c)),
                        _ => None,
                    },
                    _ => None,
                };
                match folded {
                    Some((x, k, c @ (1 | 2 | 4 | 8))) => {
                        acc.absorbed.push(id);
                        absorb_constant(ctx, &mut acc, x, k, x);
                        match c {
                            1 => acc.plain(x)?,
                            2 => acc.scaled(x, 2)?,
                            4 => acc.scaled(x, 4)?,
                            _ => acc.scaled(x, 8)?,
                        }
                    }
                    _ => acc.plain(id)?,
                }
            }
            IrOp::Const(c) if !is_root => {
                // The displacement rematerializes this use. Other consumers
                // and snapshots can still use the constant's own definition.
                if ctx.absorbable(id) {
                    acc.absorbed.push(id);
                }
                acc.disp(*c)?;
            }
            _ => {
                if is_root {
                    return Err(AddrRefusal::NotAnAddress(root));
                }
                acc.plain(id)?;
            }
        }
    }

    acc.addr.check().map_err(AddrRefusal::Unencodable)?;
    Ok(AddrMatch {
        addr: acc.addr,
        absorbed: acc.absorbed,
    })
}

// ── The load-folding gate ────────────────────────────────────────────────

/// Why a load may not be folded into its consumer's memory operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldRefusal {
    /// One of the two nodes is not in the block being selected.
    NotInBlock(NodeId),
    /// The load is not scheduled before its user, so there is nothing to fold
    /// *forward*.
    NotBefore { load: NodeId, user: NodeId },
    /// The load's value has more than one consumer (or none), so folding would
    /// duplicate the access or drop a live value.
    MultipleUses { load: NodeId, uses: u32 },
    /// A safepoint snapshot names the loaded value; it must exist in a place
    /// the deopt writer can point at.
    SafepointPinned(NodeId),
    /// The node is not a plain read: it writes, allocates, is a safepoint, or
    /// carries JMM ordering (a volatile read is an acquire and pins everything
    /// after it).
    NotAPlainRead(NodeId),
    /// A node between the load and its user refuses to be crossed.
    Intervening {
        between: NodeId,
        reason: ReorderBlock,
    },
}

/// May the value `load` produces be folded into `user`'s memory operand?
///
/// Folding turns `MOV tmp, [addr]; ADD dst, tmp` into `ADD dst, [addr]`, which
/// **moves the memory access forward** to the user's position. Every condition
/// below is about that move:
///
/// 1. Both nodes are in this block, and the load is scheduled first — the fold
///    is a forward motion or it is nothing.
/// 2. The load has exactly one value consumer. Two consumers and the fold
///    either performs the access twice (a second cache miss, and *two* reads of
///    a location another thread may be writing) or leaves the second consumer
///    without a register.
/// 3. No safepoint snapshot names the loaded value. A deopt frame has to be
///    able to name every live value; a value that only ever exists inside
///    another instruction's operand cannot be named.
/// 4. The load is a *plain* read. A volatile read is [`crate::ir::MemOrder`]
///    `Acquire`, and moving anything across it — including itself — is exactly
///    what an acquire forbids. Same for a safepointing or allocating node.
/// 5. **Every node it crosses answers `Allowed`.** This is the one that
///    matters. `Graph::may_reorder` is pairwise, so the check has to be run
///    against each intervening node individually — asking only about the user
///    would say nothing about the store in between, and folding across a store
///    to the same location makes the load read the *later* value.
///
/// Total: never panics, and every refusal names the node responsible.
pub fn may_fold_load(ctx: &SelCtx, load: NodeId, user: NodeId) -> Result<(), FoldRefusal> {
    let lp = ctx.position(load).ok_or(FoldRefusal::NotInBlock(load))?;
    let up = ctx.position(user).ok_or(FoldRefusal::NotInBlock(user))?;
    if lp >= up {
        return Err(FoldRefusal::NotBefore { load, user });
    }
    let n = ctx.uses.count(load);
    if n != 1 {
        return Err(FoldRefusal::MultipleUses { load, uses: n });
    }
    if ctx.uses.is_pinned(load) {
        return Err(FoldRefusal::SafepointPinned(load));
    }
    let eff = ctx.graph.memory_effect(load);
    if !eff.reads.is_some()
        || eff.is_write()
        || eff.safepoint
        || eff.allocates
        || !eff.order.is_plain()
    {
        return Err(FoldRefusal::NotAPlainRead(load));
    }
    // `lp + 1 <= up <= order.len()`, so the slice exists; `get` rather than
    // indexing anyway, because a codegen path must not be able to panic.
    let between = match ctx.order.get(lp + 1..up) {
        Some(s) => s,
        None => return Err(FoldRefusal::NotBefore { load, user }),
    };
    for &mid in between {
        match ctx.graph.may_reorder(load, mid) {
            Reorder::Allowed(_) => {}
            Reorder::Blocked(reason) => {
                return Err(FoldRefusal::Intervening {
                    between: mid,
                    reason,
                })
            }
        }
    }
    Ok(())
}

/// The array guards have already been proved redundant at the read.
pub(crate) fn checked_integer_array_address(
    graph: &Graph,
    checks: &crate::ir_check_elim::CheckElision,
    load: NodeId,
) -> Option<IrAddr> {
    use crate::ir::MemKind;
    if !checks.null_elided(load) || !checks.bounds_elided(load) {
        return None;
    }
    let node = graph.nodes.get(load as usize)?;
    let scale = match (&node.op, node.ty) {
        (IrOp::ArrayLoad(MemKind::Int), IrType::Int) => 4,
        (IrOp::ArrayLoad(MemKind::Long), IrType::Long) => 8,
        _ => return None,
    };
    let base = *node.inputs.get(2)?;
    let index = *node.inputs.get(3)?;
    if graph.nodes.get(base as usize)?.ty != IrType::Ref
        || graph.nodes.get(index as usize)?.ty != IrType::Int
    {
        return None;
    }
    Some(IrAddr {
        base: Some(base),
        index: Some(index),
        scale,
        disp: cratonvm_types::HEADER_SIZE as i64,
    })
}

// ── Machine instructions over virtual operands ───────────────────────────

/// Where a folded memory operand's address comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrSource {
    /// A `base + index*scale + disp` expression this module matched out of
    /// pure IR arithmetic.
    Expr(IrAddr),
    /// The address `ir_lower` already computes for this memory node.
    ///
    /// `Op::Load`'s edge layout is `[ctrl, mem, base, field_index]` — a *field
    /// index*, not a byte offset — and turning one into an `[base + disp]`
    /// operand needs the object layout, which is `ir_lower`'s knowledge and not
    /// this module's. So the selector proves the **fold** legal (which is the
    /// part that can be got wrong silently) and leaves the address shape to the
    /// lowering. This is a named node, not an absence: the lowering is told
    /// exactly which memory node's address to reuse.
    Opaque(NodeId),
}

/// One machine instruction, with IR node ids where a register will go.
///
/// This is deliberately *not* [`Req`]: a `Req` names physical registers, and
/// selection happens before allocation. [`MInst::probe`] is the bridge — it
/// runs the chosen row through the table with placeholder registers to prove an
/// encoding exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MInst {
    /// `dst <- imm`.
    Imm { dst: NodeId, ty: Ty, imm: i64 },
    /// `dst <- src`. Emitted only as the two-address fixup: an x86 ALU
    /// instruction overwrites its first operand, so a left-hand side that is
    /// still live afterwards has to be copied first.
    Move { dst: NodeId, ty: Ty, src: NodeId },
    /// `dst <- LEA addr` — no memory access, no flags.
    Lea { dst: NodeId, ty: Ty, addr: IrAddr },
    /// `dst <- lhs op rhs`, two-address (`dst` and `lhs` are the same
    /// register after allocation).
    AluRR {
        op: Op,
        ty: Ty,
        dst: NodeId,
        lhs: NodeId,
        rhs: NodeId,
    },
    /// `dst <- lhs op imm`. `form` is the field the immediate was *checked*
    /// against, never a width somebody assumed.
    AluRI {
        op: Op,
        ty: Ty,
        dst: NodeId,
        lhs: NodeId,
        imm: i64,
        form: ImmForm,
    },
    /// `dst <- lhs op [addr]` — a load folded into its single consumer.
    /// `load` names the IR node whose access this is, so the lowering knows
    /// which node it is no longer emitting separately.
    AluRM {
        op: Op,
        ty: Ty,
        dst: NodeId,
        lhs: NodeId,
        addr: AddrSource,
        load: NodeId,
    },
    /// Sign-extend the staged dividend in RAX into the implicit RDX input.
    SignExtendDividend { ty: Ty, dividend: NodeId },
    /// Divide RDX:RAX by the bound divisor. The Java guard precedes this
    /// pair. Quotient and remainder are defined in RAX and RDX respectively.
    Idiv { ty: Ty, dst: NodeId, divisor: NodeId, remainder: bool },
    /// `CMP lhs, rhs` — flags only.
    CmpRR { ty: Ty, lhs: NodeId, rhs: NodeId },
    /// `CMP lhs, imm` — flags only, immediate width checked.
    CmpRI {
        ty: Ty,
        lhs: NodeId,
        imm: i64,
        form: ImmForm,
    },
    /// `TEST reg, reg` — the compare-against-zero form. `CMP r, 0` and
    /// `TEST r, r` leave CF, OF, SF and ZF identical (one is `SUB r, 0`, the
    /// other `AND r, r`; both clear CF and OF and set SF/ZF from the value), so
    /// this substitution is exact for **every** condition code, signed or
    /// unsigned — not just the equality pair.
    TestRR { ty: Ty, reg: NodeId },
    /// `SETcc dst8; MOVZX dst, dst8` — a comparison whose 0/1 value is
    /// actually read.
    SetCc { dst: NodeId, cc: CmpOp },
    /// `Jcc` on the flags the immediately preceding compare wrote. `at` is the
    /// `Op::If` node, so the caller can find its successor edges.
    Jcc { cc: CmpOp, at: NodeId },
    /// No rule matched: `ir_lower`'s generic per-node lowering owns this node.
    ///
    /// **Not** a no-op. A selector that answered an unmatched node with
    /// silence would drop the node's semantics entirely; this arm names the
    /// node the caller must still lower.
    Generic { node: NodeId },
}

pub(crate) fn integer_division_steps(
    ty: Ty,
    dst: NodeId,
    dividend: NodeId,
    divisor: NodeId,
    remainder: bool,
) -> [MInst; 2] {
    [
        MInst::SignExtendDividend { ty, dividend },
        MInst::Idiv { ty, dst, divisor, remainder },
    ]
}

impl MInst {
    pub(crate) fn bind_divisor(&self, reg: u8) -> Result<Encoded, SelError> {
        if !matches!(self, MInst::Idiv { .. }) {
            return Err(SelError::UnknownPattern);
        }
        let row = self.pattern_name().ok_or(SelError::UnknownPattern)?;
        encode_named(row, &Args { dst: reg, ..Args::default() })
    }

    /// The [`PATTERNS`] row this instruction encodes through, when the table
    /// has one.
    ///
    /// `None` is a statement about the *table*, not about the instruction: see
    /// `docs/jit/instruction-selection.md` for the rows that are still missing
    /// and what each needs.
    pub fn pattern_name(&self) -> Option<&'static str> {
        match *self {
            MInst::Imm {
                ty: Ty::I64, imm, ..
            } => Some(match imm {
                0 => "mov_r64_imm0_xor",
                // The rows `select` picks (shortest wins): zero-extending for
                // `1..=u32::MAX`, sign-extending for the negative i32 range.
                v if u32::try_from(v).is_ok() => "mov_r32_imm32_zx",
                v if i32::try_from(v).is_ok() => "mov_r64_imm32",
                _ => "mov_r64_imm64",
            }),
            MInst::Imm { .. } => None,
            MInst::Move { ty: Ty::I64, .. } => Some("mov_r64_r64"),
            MInst::Move { ty: Ty::I32, .. } => Some("mov_r32_r32"),
            MInst::Move { .. } => None,
            MInst::Lea { ty: Ty::I64, .. } => Some("lea_r64_m"),
            MInst::Lea { ty: Ty::I32, .. } => Some("lea_r32_m"),
            MInst::Lea { .. } => None,
            MInst::AluRR { op, ty, .. } => match (op, ty) {
                (Op::Add, Ty::I64) => Some("add_r64_r64"),
                (Op::Add, Ty::I32) => Some("add_r32_r32"),
                (Op::Sub, Ty::I64) => Some("sub_r64_r64"),
                (Op::Sub, Ty::I32) => Some("sub_r32_r32"),
                (Op::And, Ty::I64) => Some("and_r64_r64"),
                (Op::And, Ty::I32) => Some("and_r32_r32"),
                (Op::Or, Ty::I64) => Some("or_r64_r64"),
                (Op::Or, Ty::I32) => Some("or_r32_r32"),
                (Op::Xor, Ty::I64) => Some("xor_r64_r64"),
                (Op::Xor, Ty::I32) => Some("xor_r32_r32"),
                (Op::Imul, Ty::I64) => Some("imul_r64_r64"),
                (Op::Imul, Ty::I32) => Some("imul_r32_r32"),
                (Op::Shl, Ty::I32) => Some("shl_r32_cl"),
                (Op::Shl, Ty::I64) => Some("shl_r64_cl"),
                (Op::Shr, Ty::I32) => Some("shr_r32_cl"),
                (Op::Shr, Ty::I64) => Some("shr_r64_cl"),
                (Op::Sar, Ty::I32) => Some("sar_r32_cl"),
                (Op::Sar, Ty::I64) => Some("sar_r64_cl"),
                _ => None,
            },
            MInst::AluRI {
                op,
                ty: Ty::I64,
                form,
                ..
            } => match (op, form) {
                (Op::Add, ImmForm::Imm8) => Some("add_r64_imm8"),
                (Op::Add, ImmForm::Imm32) => Some("add_r64_imm32"),
                (Op::Sub, ImmForm::Imm8) => Some("sub_r64_imm8"),
                (Op::Sub, ImmForm::Imm32) => Some("sub_r64_imm32"),
                (Op::And, ImmForm::Imm8) => Some("and_r64_imm8"),
                (Op::Or, ImmForm::Imm8) => Some("or_r64_imm8"),
                (Op::And, ImmForm::Imm32) => Some("and_r64_imm32"),
                (Op::Or, ImmForm::Imm32) => Some("or_r64_imm32"),
                (Op::Xor, ImmForm::Imm8) => Some("xor_r64_imm8"),
                (Op::Xor, ImmForm::Imm32) => Some("xor_r64_imm32"),
                (Op::Imul, ImmForm::Imm8) => Some("imul_r64_r64_imm8"),
                (Op::Imul, ImmForm::Imm32) => Some("imul_r64_r64_imm32"),
                (Op::Shl, ImmForm::ImmU8) => Some("shl_r64_imm8"),
                (Op::Shr, ImmForm::ImmU8) => Some("shr_r64_imm8"),
                (Op::Sar, ImmForm::ImmU8) => Some("sar_r64_imm8"),
                _ => None,
            },
            // 32-bit, the forms real Java arithmetic actually asks for. `AluImm`
            // fired zero times on 850 Spring Boot compiles until these existed:
            // the rows were missing AND this mapping was, and either alone is
            // enough to make `require_encodable` discard the tile.
            MInst::AluRI {
                op,
                ty: Ty::I32,
                form,
                ..
            } => match (op, form) {
                (Op::Add, ImmForm::Imm8) => Some("add_r32_imm8"),
                (Op::Add, ImmForm::Imm32) => Some("add_r32_imm32"),
                (Op::Sub, ImmForm::Imm8) => Some("sub_r32_imm8"),
                (Op::Sub, ImmForm::Imm32) => Some("sub_r32_imm32"),
                (Op::And, ImmForm::Imm8) => Some("and_r32_imm8"),
                (Op::Or, ImmForm::Imm8) => Some("or_r32_imm8"),
                (Op::Xor, ImmForm::Imm8) => Some("xor_r32_imm8"),
                (Op::And, ImmForm::Imm32) => Some("and_r32_imm32"),
                (Op::Or, ImmForm::Imm32) => Some("or_r32_imm32"),
                (Op::Xor, ImmForm::Imm32) => Some("xor_r32_imm32"),
                (Op::Imul, ImmForm::Imm8) => Some("imul_r32_r32_imm8"),
                (Op::Imul, ImmForm::Imm32) => Some("imul_r32_r32_imm32"),
                (Op::Shl, ImmForm::ImmU8) => Some("shl_r32_imm8"),
                (Op::Shr, ImmForm::ImmU8) => Some("shr_r32_imm8"),
                (Op::Sar, ImmForm::ImmU8) => Some("sar_r32_imm8"),
                _ => None,
            },
            MInst::AluRI { .. } => None,
            MInst::AluRM { op, ty, addr: AddrSource::Expr(_), .. } => match (op, ty) {
                (Op::Add, Ty::I32) => Some("add_r32_m"),
                (Op::Add, Ty::I64) => Some("add_r64_m"),
                (Op::Sub, Ty::I32) => Some("sub_r32_m"),
                (Op::Sub, Ty::I64) => Some("sub_r64_m"),
                (Op::And, Ty::I32) => Some("and_r32_m"),
                (Op::And, Ty::I64) => Some("and_r64_m"),
                (Op::Or, Ty::I32) => Some("or_r32_m"),
                (Op::Or, Ty::I64) => Some("or_r64_m"),
                (Op::Xor, Ty::I32) => Some("xor_r32_m"),
                (Op::Xor, Ty::I64) => Some("xor_r64_m"),
                (Op::Imul, Ty::I32) => Some("imul_r32_m"),
                (Op::Imul, Ty::I64) => Some("imul_r64_m"),
                _ => None,
            },
            MInst::AluRM { .. } => None,
            MInst::SignExtendDividend { ty: Ty::I32, .. } => Some("cdq"),
            MInst::SignExtendDividend { ty: Ty::I64, .. } => Some("cqo"),
            MInst::Idiv { ty: Ty::I32, .. } => Some("idiv_r32"),
            MInst::Idiv { ty: Ty::I64, .. } => Some("idiv_r64"),
            MInst::SignExtendDividend { .. } | MInst::Idiv { .. } => None,
            MInst::CmpRR { ty: Ty::I64, .. } => Some("cmp_r64_r64"),
            MInst::CmpRR { ty: Ty::I32, .. } => Some("cmp_r32_r32"),
            MInst::CmpRR { .. } => None,
            MInst::CmpRI { ty, form, .. } => match (ty, form) {
                (Ty::I32, ImmForm::Imm8) => Some("cmp_r32_imm8"),
                (Ty::I32, ImmForm::Imm32) => Some("cmp_r32_imm32"),
                (Ty::I64, ImmForm::Imm8) => Some("cmp_r64_imm8"),
                (Ty::I64, ImmForm::Imm32) => Some("cmp_r64_imm32"),
                _ => None,
            },
            MInst::TestRR { ty: Ty::I64, .. } => Some("test_r64_r64"),
            MInst::TestRR { ty: Ty::I32, .. } => Some("test_r32_r32"),
            MInst::TestRR { .. } => None,
            MInst::SetCc { .. } => None,
            MInst::Jcc { .. } => Some("jcc_rel32"),
            MInst::Generic { .. } => None,
        }
    }

    /// Prove an encoding exists, using placeholder registers.
    ///
    /// Register *numbers* change an encoding's length (REX, the RSP/RBP
    /// addressing quirks) but not whether one exists for these rows, which is
    /// the only question a selector can answer before allocation. The
    /// displacement and the immediate are the operands that *can* make a row
    /// inapplicable, and those are real here, so this catches the mistakes that
    /// matter: an immediate that does not fit its field, a displacement past
    /// `disp32`, a scale that is not 1/2/4/8.
    pub fn probe(&self) -> Result<Encoded, SelError> {
        let name = self.pattern_name().ok_or(SelError::UnknownPattern)?;
        let mut a = Args {
            dst: RAX,
            src: RCX,
            mem: Mem::base_disp(RAX, 0),
            imm: 0,
            op_byte: 0,
        };
        match *self {
            MInst::Idiv { .. } => a.dst = RCX,
            MInst::Imm { imm, .. } => a.imm = imm,
            MInst::Move { .. } => {}
            MInst::Lea { addr, .. }
            | MInst::AluRM {
                addr: AddrSource::Expr(addr),
                ..
            } => {
                addr.check()?;
                a.mem = Mem {
                    base: RAX,
                    index: addr.index.map(|_| Index {
                        reg: RCX,
                        scale: addr.scale,
                    }),
                    disp: addr.disp,
                    force_disp32: false,
                };
            }
            MInst::AluRI { imm, .. } | MInst::CmpRI { imm, .. } => a.imm = imm,
            MInst::Jcc { cc, .. } => a.op_byte = cc.x64_cc(),
            // The `TEST r, r` rows declare `Constraint::SameRegister`: the
            // instruction *is* "compare this value with itself". Probing them
            // with two different placeholder registers would fail the row's own
            // constraint and report a missing encoding that is not missing.
            MInst::TestRR { .. } => a.src = a.dst,
            _ => {}
        }
        encode_named(name, &a)
    }

    /// This instruction's cost.
    ///
    /// Taken from the [`PATTERNS`] row whenever the table has one, so the cost
    /// model and the encoder cannot drift; the literals below are only for the
    /// instructions the table does not cover yet, and each states its own
    /// arithmetic.
    pub fn cost(&self) -> SeqCost {
        if let Some(row) = self.pattern_name().and_then(|n| pattern(n)) {
            // The table's `bytes` is the operand-independent floor. An address
            // with an index or a displacement is longer than the floor, and
            // the cost model has to see that or it will fold a disp32 into a
            // memory operand as though it were free.
            let extra = match *self {
                MInst::Lea { addr, .. }
                | MInst::AluRM {
                    addr: AddrSource::Expr(addr),
                    ..
                } => addr.operand_bytes().saturating_sub(1),
                _ => 0,
            };
            let mut seq = SeqCost::of(row.cost);
            seq.bytes = seq.bytes.saturating_add(extra);
            return seq;
        }
        match *self {
            // `CMP r/m, imm8` is `83 /7 ib`, `imm32` is `81 /7 id`; add one
            // REX byte for the 64-bit forms.
            MInst::CmpRI { ty, form, .. } => {
                let rex = u32::from(matches!(ty, Ty::I64));
                let imm = u32::try_from(form.byte_len()).unwrap_or(8);
                SeqCost {
                    bytes: 2 + rex + imm,
                    uops: 1,
                    latency: 1,
                }
            }
            // `OP r, [mem]` is one *fused* micro-op on every x86-64 core that
            // matters, which is the whole reason to fold: the byte count barely
            // moves, the micro-op count halves. Latency is a load (≈4) plus the
            // ALU operation.
            MInst::AluRM { ty, addr, .. } => {
                let rex = u32::from(matches!(ty, Ty::I64));
                let operand = match addr {
                    AddrSource::Expr(a) => a.operand_bytes(),
                    // Unknown address shape: price it as the widest common
                    // form (ModRM + disp32) rather than as the cheapest.
                    AddrSource::Opaque(_) => 5,
                };
                SeqCost {
                    bytes: 1 + rex + operand,
                    uops: 1,
                    latency: 5,
                }
            }
            // `0F 9x C0` then `0F B6 C0`.
            MInst::SetCc { .. } => SeqCost {
                bytes: 6,
                uops: 2,
                latency: 2,
            },
            // Everything else the table does not cover is priced as the
            // generic lowering, which is the direction that never makes an
            // unmodelled instruction look attractive.
            _ => SeqCost::of(GENERIC_COST),
        }
    }
}

// ── Tiles ────────────────────────────────────────────────────────────────

/// Which rule produced a tile. Diagnostic, and the handle the tests use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// No rule matched; `ir_lower` lowers this node the usual way.
    Generic,
    /// An add / shift / multiply tree collapsed into one `LEA`.
    Lea,
    /// A binary op whose right operand is a constant that fits the immediate
    /// field.
    AluImm,
    /// A binary op on two registers.
    AluReg,
    /// A load folded into its single arithmetic consumer.
    AluFoldedLoad,
    /// An `Op::Cmp` fused with the `Op::If` it feeds: `CMP; Jcc`.
    CmpBranch,
    /// The same, against a zero operand: `TEST; Jcc`.
    TestZeroBranch,
    /// An `Op::If` whose condition is not a fusable compare: `TEST; Jcc`,
    /// which is what `ir_lower` already emits.
    TestBranch,
    /// An `Op::Cmp` whose 0/1 value is read: `CMP; SETcc; MOVZX`.
    CmpSetCc,
    /// The fixed-register machine steps of a guarded variable integer divide.
    GuardedDiv,
}

/// One IR subtree and the machine instructions selected for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tile {
    /// The node this tile computes.
    pub root: NodeId,
    /// Every IR node this tile subsumes, `root` included. No other tile may
    /// cover any of them, and the caller must not lower them separately.
    pub covered: Vec<NodeId>,
    /// The instructions, in emission order.
    pub insts: Vec<MInst>,
    /// Sum of the instruction costs.
    pub cost: SeqCost,
    /// Which rule fired.
    pub rule: Rule,
}

/// What one operand costs to bring in from its frame word: `MOV r64, [RBP -
/// disp8]`, four bytes and one micro-op, at a load's latency.
///
/// The disp8 form deliberately — it is the common case and the *smaller*
/// figure, so a cost model that consults this never over-states what an
/// immediate form saves.
const FRAME_LOAD_COST: SeqCost = SeqCost {
    bytes: 4,
    uops: 1,
    latency: 4,
};

impl Tile {
    fn new(root: NodeId, covered: Vec<NodeId>, insts: Vec<MInst>, rule: Rule) -> Tile {
        let cost = insts
            .iter()
            .fold(SeqCost::default(), |acc, i| acc.then(i.cost()));
        Tile {
            root,
            covered,
            insts,
            cost,
            rule,
        }
    }

    /// What the nodes this tile covers would have cost had each been left to
    /// the generic lowering.
    ///
    /// One [`GENERIC_COST`] per covered node. That is the *alternative* to
    /// covering them: a node a tile absorbs is a node the tiler never visits,
    /// so it never gets a tile of its own.
    pub fn baseline(&self) -> SeqCost {
        let n = u32::try_from(self.covered.len()).unwrap_or(u32::MAX);
        SeqCost {
            bytes: u32::from(GENERIC_COST.bytes).saturating_mul(n),
            uops: u32::from(GENERIC_COST.uops).saturating_mul(n),
            latency: u32::from(GENERIC_COST.latency).saturating_mul(n),
        }
    }

    /// The ranking key: this tile's cost **minus** what it displaces.
    ///
    /// Raw cost alone cannot rank a tiling. Folding a load into its consumer
    /// makes the *consumer* more expensive — `ADD r, [m]` is longer and slower
    /// than `ADD r, r` — and is still the right choice, because the load's own
    /// four instructions disappear. Only the net figure sees that.
    ///
    /// Signed, not saturating: two tiles that cover the same nodes have the
    /// same baseline, so the raw costs still separate them. A saturating
    /// subtraction would clamp both to zero and make the choice arbitrary.
    ///
    /// Micro-ops first, then bytes, then latency — see [`SeqCost::key`].
    pub fn net_key(&self) -> (i64, i64, i64) {
        let b = self.baseline();
        (
            i64::from(self.cost.uops) - i64::from(b.uops),
            i64::from(self.cost.bytes) - i64::from(b.bytes),
            i64::from(self.cost.latency) - i64::from(b.latency),
        )
    }

    /// This tile's cost, re-priced for a frame-homed allocation.
    ///
    /// [`MInst::cost`] prices instructions. Under
    /// [`SelectOptions::frame_homed`] an operand that stays a register is also
    /// a `MOV r64, [RBP - disp]` the consumer has to emit to bring it in, and
    /// two candidates for one node can need a *different number* of those:
    /// `ADD EAX, ECX` loads two values where `ADD EAX, 7` loads one. Pricing
    /// only the instruction hides four bytes and a micro-op, and hands the node
    /// to the register form every time.
    ///
    /// Every tile loads at least one operand — the value it computes from — so
    /// the first is free here and only the extras are charged. That keeps this
    /// a *comparison between candidates for one node* rather than an absolute
    /// figure competing with [`GENERIC_COST`].
    ///
    /// The operand set is a set: `x + x` selects an `LEA [x + x]` that loads
    /// `x` once, and counting edges rather than values would charge it twice.
    fn frame_homed(mut self) -> Tile {
        let mut operands: Vec<NodeId> = Vec::new();
        for inst in &self.insts {
            let mut note = |id: NodeId| {
                if !operands.contains(&id) {
                    operands.push(id);
                }
            };
            match *inst {
                MInst::Imm { .. } | MInst::Generic { .. } | MInst::Jcc { .. } => {}
                MInst::Move { src, .. } => note(src),
                MInst::Lea { addr, .. } => {
                    if let Some(b) = addr.base {
                        note(b);
                    }
                    if let Some(i) = addr.index {
                        note(i);
                    }
                }
                MInst::AluRR { lhs, rhs, .. } => {
                    note(lhs);
                    note(rhs);
                }
                MInst::AluRI { lhs, .. } => note(lhs),
                // A folded load's address is the memory node's own, which the
                // lowering computes; only the kept operand is a frame word.
                MInst::AluRM { lhs, .. } => note(lhs),
                MInst::SignExtendDividend { dividend, .. } => note(dividend),
                MInst::Idiv { divisor, .. } => note(divisor),
                MInst::CmpRR { lhs, rhs, .. } => {
                    note(lhs);
                    note(rhs);
                }
                MInst::CmpRI { lhs, .. } => note(lhs),
                MInst::TestRR { reg, .. } => note(reg),
                // `SETcc` reads flags, not a frame word.
                MInst::SetCc { .. } => {}
            }
        }
        for _ in 1..operands.len() {
            self.cost = self.cost.then(FRAME_LOAD_COST);
        }
        self
    }

    /// The fall-back tile: one node, lowered the old way.
    pub fn generic(root: NodeId) -> Tile {
        Tile::new(
            root,
            vec![root],
            vec![MInst::Generic { node: root }],
            Rule::Generic,
        )
    }

    // `Tile::for_test` (the tests-only constructor) is defined in this file's
    // bottom test module, so no test gate sits above production code here.

    /// Can the pattern table encode every instruction in this tile?
    ///
    /// `Rule::Generic` is encodable by definition — it delegates to the
    /// hand-written lowering, which is not the table's business.
    pub fn encodable(&self) -> Result<(), SelError> {
        if matches!(self.rule, Rule::Generic) {
            return Ok(());
        }
        for i in &self.insts {
            i.probe()?;
        }
        Ok(())
    }
}

/// Knobs for [`select_block`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectOptions {
    /// Discard any tile whose instructions [`PATTERNS`] cannot encode.
    ///
    /// `true` — the default, and the only setting a production wiring may use
    /// — makes the selector refuse to choose an instruction it cannot prove
    /// exists. `false` reports the tiling the rules *would* pick, which is how
    /// the tests enumerate the rows the table is still missing.
    pub require_encodable: bool,
    /// Fold a load into its single arithmetic consumer. Off by default: the
    /// gate is sound (see [`may_fold_load`]) but the *address* half is still
    /// [`AddrSource::Opaque`], so a lowering has to teach the memory operand to
    /// `ir_lower` before this is worth turning on.
    pub fold_loads: bool,
    /// The consumer will encode these tiles against a **frame-homed**
    /// allocation: every value lives in its frame word, and an instruction's
    /// operands are loaded into scratch registers on the spot.
    ///
    /// This is not a hint, it is a statement about the allocation, and it
    /// changes what the cost model is measuring. The two-address fixup — the
    /// `MInst::Move` that copies a still-live left operand before an x86 ALU
    /// instruction overwrites it — does not exist under frame homing: the
    /// destination's register never *held* the left operand, so "copy it there"
    /// and "load it there" are the same instruction, and the ALU form pays
    /// nothing for a live left operand.
    ///
    /// Off by default, deliberately. Increments 0 and 1 measured coverage with
    /// it off; flipping the default would silently re-base those figures.
    ///
    /// What it decides, concretely: with a live left operand and this `false`,
    /// `Rule::Lea` outbids `Rule::AluReg` on the strength of a copy the
    /// consumer would never have emitted, and `a + b` selects an `LEA` that is
    /// a byte longer than the `ADD` it replaced.
    pub frame_homed: bool,
    /// The consumer handles Java integer division guards and uses the
    /// selected fixed-register steps for the passing path.
    pub allocated_division: bool,
    /// The consumer handles materialized comparisons (`MInst::SetCc`)
    /// through allocated register or frame emission.
    pub allocated_setcc: bool,
}

impl Default for SelectOptions {
    fn default() -> SelectOptions {
        SelectOptions {
            require_encodable: true,
            fold_loads: false,
            frame_homed: false,
            allocated_division: false,
            allocated_setcc: false,
        }
    }
}

/// Something a rule declined, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// An address expression was rejected.
    Address { root: NodeId, why: AddrRefusal },
    /// A load fold was rejected.
    Fold { user: NodeId, why: FoldRefusal },
    /// A tile was rejected because the table cannot encode it.
    Unencodable {
        root: NodeId,
        rule: Rule,
        why: SelError,
    },
    /// A constant did not fit any immediate field.
    WideImmediate { root: NodeId, value: i64 },
}

/// The tiling of one basic block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockSelection {
    /// The tiles, in emission order: the block's data nodes first (in
    /// scheduled order, skipping the ones a later tile absorbed), then the
    /// terminator's tile.
    pub tiles: Vec<Tile>,
    /// Total cost.
    pub cost: SeqCost,
    /// Every rule refusal, for diagnostics and for the tests.
    pub notes: Vec<Note>,
}

impl BlockSelection {
    /// Does this selection cover `block` exactly once per node?
    ///
    /// The invariant the whole design rests on: a node covered twice is
    /// computed twice, and a node covered zero times is a dropped instruction —
    /// which is the failure mode a catch-all `_ => {}` produces and the reason
    /// this check exists.
    pub fn covers(&self, block: &[NodeId]) -> bool {
        let mut seen: Vec<NodeId> = self
            .tiles
            .iter()
            .flat_map(|t| t.covered.iter().copied())
            .collect();
        let mut want: Vec<NodeId> = block.to_vec();
        seen.sort_unstable();
        want.sort_unstable();
        // The terminator's own id is allowed to appear in `seen` without being
        // in `block`: it is a control node and never a member of `Block::nodes`.
        seen.retain(|id| want.binary_search(id).is_ok());
        seen == want
    }

    /// Tiles that fired a rule other than [`Rule::Generic`].
    pub fn matched(&self) -> impl Iterator<Item = &Tile> + '_ {
        self.tiles.iter().filter(|t| t.rule != Rule::Generic)
    }
}

// ── Rules ────────────────────────────────────────────────────────────────

/// The narrowest immediate field that can hold `value`.
///
/// `i8::try_from` / `i32::try_from`, never `as i8` / `as i32`. A raw narrowing
/// here is a wrong-code bug of exactly the shape `disp.rs` was written to
/// close: `128 as i8` is `-128`, and an `AND r64, -128` where `AND r64, 128`
/// was meant clears the wrong bits.
pub fn imm_form_for(value: i64) -> Option<ImmForm> {
    if i8::try_from(value).is_ok() {
        Some(ImmForm::Imm8)
    } else if i32::try_from(value).is_ok() {
        Some(ImmForm::Imm32)
    } else {
        None
    }
}

/// The machine operation an IR binary node performs, and the operand type to
/// perform it at.
///
/// Every op is performed at the node's own width, the bitwise three included.
/// They used to come back as [`Ty::I64`] for an `Int` node, on the reasoning
/// that "the low 32 bits of a 64-bit bitwise operation are the 32-bit result".
/// The low half is — the *high* half is not. `ir_lower`'s `Int` arms keep the
/// tier's convention that an int's home word is the zero-extension of its low
/// half, and a REX.W `AND`/`OR`/`XOR` over two operands that disagree about
/// that half (one sign-extended out of `iaload`/`F2I`/a negative `Const`, one
/// zero-extended from any 32-bit ALU result) produces a word that is neither
/// — `0xFFFF_FFFF_0000_0006` — which is an out-of-bounds element address that
/// passes a low-half bounds check. `ir_lower` fixed its own arms to the 32-bit
/// form for exactly that reason (see its `Op::And` comment); the selector kept
/// choosing `and_r64_r64`, so under `CRATONVM_JIT=ir-isel-emit` an `Int` `&`,
/// `|` or `^` tile re-introduced the bug, and under `ir-isel-verify` every one
/// of them was a byte mismatch that refused the compile. `OR r64, imm8` with a
/// negative immediate is the same defect with an immediate: `x | -1` set all
/// 64 bits of an `Int`.
fn alu_op_of(op: &IrOp, ty: Ty) -> Option<(Op, Ty)> {
    Some(match op {
        IrOp::Add => (Op::Add, ty),
        IrOp::Sub => (Op::Sub, ty),
        IrOp::Mul => (Op::Imul, ty),
        IrOp::And => (Op::And, ty),
        IrOp::Or => (Op::Or, ty),
        IrOp::Xor => (Op::Xor, ty),
        IrOp::Shl => (Op::Shl, ty),
        IrOp::Shr => (Op::Sar, ty),
        IrOp::UShr => (Op::Shr, ty),
        _ => return None,
    })
}

/// Is this operation commutative, i.e. may the selector fold either operand?
fn commutative(op: &IrOp) -> bool {
    matches!(op, IrOp::Add | IrOp::Mul | IrOp::And | IrOp::Or | IrOp::Xor)
}

/// Does the left-hand side need copying before a two-address ALU instruction
/// overwrites it?
///
/// `dst <- lhs op rhs` compiles to `OP dst, rhs` with `dst` and `lhs` coalesced.
/// That is only legal when `lhs` dies at this node — one consumer, and no deopt
/// frame naming it. Otherwise the tile pays for a `MOV` first, and that cost is
/// exactly what makes the non-destructive `LEA` win the comparison.
///
/// Under [`SelectOptions::frame_homed`] there is no coalescing to protect:
/// the destination's register never held the left operand, so the consumer
/// loads it either way and the copy is not a copy. Answering `true` there would
/// price a `MOV` nobody emits — and that fiction is what makes `LEA` outbid the
/// `ADD` it is a byte longer than.
fn needs_copy(ctx: &SelCtx, lhs: NodeId, opts: &SelectOptions) -> bool {
    !opts.frame_homed && !ctx.uses.single_use(lhs)
}

/// `LEA` for an add / shift / multiply tree.
fn tile_lea(ctx: &SelCtx, root: NodeId, claimed: &[bool], notes: &mut Vec<Note>) -> Option<Tile> {
    // 32-bit `LEA` (`8D /r` with REX.W clear) is correct for `int` arithmetic:
    // it truncates to 32 bits, which is exactly Java's wrap, and zero-extends
    // into the destination — the same high half `ADD EAX, ECX` leaves, which is
    // what `ir_lower`'s `Op::Add`/`Op::Mul` `Int` arms already emit. The row it
    // encodes through (`lea_r32_m`) is anchored to `emit_imul_const`'s
    // `8D 04 40` / `8D 04 80` / `8D 04 C0`. Nothing wider than `I64` gets here:
    // `int_ty` maps only `Int`/`Long`/`Ref`.
    let ty = ctx.int_ty(root)?;
    let m = match match_address(ctx, root) {
        Ok(m) => m,
        // `NotAnAddress` only means "this rule does not apply to this node",
        // which is true of most nodes and is not worth a note. Everything else
        // is a rule that *wanted* to fire and could not, which is.
        Err(AddrRefusal::NotAnAddress(_)) => return None,
        Err(why) => {
            notes.push(Note::Address { root, why });
            return None;
        }
    };
    // One term is a copy or an add, not an address.
    if m.addr.terms() < 2 {
        return None;
    }
    for &id in &m.absorbed {
        if id != root && claimed.get(id as usize).copied().unwrap_or(true) {
            return None;
        }
    }
    Some(Tile::new(
        root,
        m.absorbed.clone(),
        vec![MInst::Lea {
            dst: root,
            ty,
            addr: m.addr,
        }],
        Rule::Lea,
    ))
}

/// Java guards remain in IR; this tile owns the sign extension and divide.
fn tile_integer_division(ctx: &SelCtx, root: NodeId) -> Option<Tile> {
    let node = ctx.graph.node_opt(root)?;
    let remainder = match node.op {
        IrOp::Div => false,
        IrOp::Rem => true,
        _ => return None,
    };
    let ty = match node.ty {
        IrType::Int => Ty::I32,
        IrType::Long => Ty::I64,
        _ => return None,
    };
    let dividend = node.input_opt(0)?;
    let divisor = node.input_opt(1)?;
    ctx.graph.node_opt(dividend)?;
    ctx.graph.node_opt(divisor)?;
    if ctx.const_of(divisor).is_some() {
        return None;
    }
    Some(Tile::new(
        root,
        vec![root],
        integer_division_steps(ty, root, dividend, divisor, remainder).to_vec(),
        Rule::GuardedDiv,
    ))
}

/// The register / immediate / folded-load forms of a binary integer node.
fn tiles_alu(
    ctx: &SelCtx,
    root: NodeId,
    claimed: &[bool],
    opts: &SelectOptions,
    notes: &mut Vec<Note>,
) -> Vec<Tile> {
    let mut out = Vec::new();
    let node = match ctx.graph.node_opt(root) {
        Some(n) => n,
        None => return out,
    };
    let ty = match ctx.int_ty(root) {
        Some(t) => t,
        None => return out,
    };
    let (op, opty) = match alu_op_of(&node.op, ty) {
        Some(p) => p,
        None => return out,
    };
    let (lhs, rhs) = match (node.input_opt(0), node.input_opt(1)) {
        (Some(a), Some(b)) => (a, b),
        _ => return out,
    };

    // The two-address fixup copies the *whole* register: there is no such
    // thing as copying half a value, and `ir_lower` stores every value in a
    // 64-bit frame slot. Always `Ty::I64`, so the copy has a table row for
    // both `int` and `long` operands.
    let prefix = |lhs: NodeId| -> Vec<MInst> {
        if needs_copy(ctx, lhs, opts) {
            vec![MInst::Move {
                dst: root,
                ty: Ty::I64,
                src: lhs,
            }]
        } else {
            Vec::new()
        }
    };

    // ── constant into the immediate field ────────────────────────────────
    //
    // `Sub` is not commutative, so only its right operand may become an
    // immediate; the others may take either.
    let const_side = match (ctx.const_of(lhs), ctx.const_of(rhs)) {
        (_, Some(c)) => Some((lhs, rhs, c)),
        (Some(c), None) if commutative(&node.op) => Some((rhs, lhs, c)),
        _ => None,
    };
    let shift = matches!(op, Op::Shl | Op::Shr | Op::Sar);
    if let Some((reg, kn, c)) = const_side {
        let (c, form) = if shift {
            let mask = if opty == Ty::I64 { 63 } else { 31 };
            (c & mask, Some(ImmForm::ImmU8))
        } else {
            (c, imm_form_for(c))
        };
        match form {
            Some(form) => {
                let mut covered = vec![root];
                if ctx.absorbable(kn) && !claimed.get(kn as usize).copied().unwrap_or(true) {
                    covered.push(kn);
                }
                let mut insts = prefix(reg);
                insts.push(MInst::AluRI {
                    op,
                    ty: opty,
                    dst: root,
                    lhs: reg,
                    imm: c,
                    form,
                });
                out.push(Tile::new(root, covered, insts, Rule::AluImm));
            }
            None => notes.push(Note::WideImmediate { root, value: c }),
        }
    }

    if shift && const_side.is_some() {
        return out;
    }

    // ── a load folded into the memory operand ────────────────────────────
    if !shift && opts.fold_loads {
        // `Sub` may only fold its right operand: `SUB dst, [m]` is
        // `dst - [m]`, and there is no `[m] - dst` form.
        let mut candidates: Vec<(NodeId, NodeId)> = vec![(lhs, rhs)];
        if commutative(&node.op) {
            candidates.push((rhs, lhs));
        }
        for (keep, fold) in candidates {
            match may_fold_load(ctx, fold, root) {
                Ok(()) => {
                    if claimed.get(fold as usize).copied().unwrap_or(true) {
                        continue;
                    }
                    let addr = match &ctx.checked_array_reads {
                        Some(reads) => {
                            let Some(addr) = reads.get(fold as usize).copied().flatten() else {
                                continue;
                            };
                            AddrSource::Expr(addr)
                        }
                        None => AddrSource::Opaque(fold),
                    };
                    let mut insts = prefix(keep);
                    insts.push(MInst::AluRM {
                        op,
                        ty: opty,
                        dst: root,
                        lhs: keep,
                        addr,
                        load: fold,
                    });
                    out.push(Tile::new(
                        root,
                        vec![root, fold],
                        insts,
                        Rule::AluFoldedLoad,
                    ));
                    break;
                }
                Err(why) => notes.push(Note::Fold { user: root, why }),
            }
        }
    }

    // ── plain register form ──────────────────────────────────────────────
    let mut insts = prefix(lhs);
    insts.push(MInst::AluRR {
        op,
        ty: opty,
        dst: root,
        lhs,
        rhs,
    });
    out.push(Tile::new(root, vec![root], insts, Rule::AluReg));
    out
}

/// The operand a compare absorbed into its immediate field or into `TEST`,
/// as a (possibly empty) cover list.
fn absorbed_operand(ctx: &SelCtx, id: NodeId, claimed: &[bool]) -> Vec<NodeId> {
    if ctx.absorbable(id) && !claimed.get(id as usize).copied().unwrap_or(true) {
        vec![id]
    } else {
        Vec::new()
    }
}

/// The flag-setting half of a comparison, shared by the fused-branch and the
/// materialising forms.
///
/// Returns the instructions plus the extra node the compare absorbed (the zero
/// constant, when `TEST` replaced `CMP r, 0`).
fn compare_insts(
    ctx: &SelCtx,
    cmp: NodeId,
    claimed: &[bool],
) -> Option<(Vec<MInst>, Vec<NodeId>, bool)> {
    let node = ctx.graph.node_opt(cmp)?;
    let (a, b) = (node.input_opt(0)?, node.input_opt(1)?);
    // A reference comparison must compare all 64 bits: two distinct objects
    // 4 GiB apart agree in their low word, and so does a heap pointer whose low
    // word happens to be zero and `null`. `int_ty` already maps `Ref` to
    // `Ty::I64`; take the wider of the two operand types so a mixed pair
    // (a `Ref` against an `Int`-typed null constant) still compares wide.
    let ty = match (ctx.int_ty(a), ctx.int_ty(b)) {
        (Some(Ty::I64), _) | (_, Some(Ty::I64)) => Ty::I64,
        (Some(t), _) => t,
        (_, Some(t)) => t,
        _ => return None,
    };
    // `CMP r, 0` -> `TEST r, r`.
    if ctx.const_of(b) == Some(0) {
        let extra = absorbed_operand(ctx, b, claimed);
        return Some((vec![MInst::TestRR { ty, reg: a }], extra, false));
    }
    if ctx.const_of(a) == Some(0) {
        let extra = absorbed_operand(ctx, a, claimed);
        // The operands swapped, so the condition has to be mirrored by the
        // caller; report that rather than silently comparing backwards.
        return Some((vec![MInst::TestRR { ty, reg: b }], extra, true));
    }
    // `CMP r, imm`.
    if let Some(c) = ctx.const_of(b) {
        if let Some(form) = imm_form_for(c) {
            let extra = absorbed_operand(ctx, b, claimed);
            return Some((
                vec![MInst::CmpRI {
                    ty,
                    lhs: a,
                    imm: c,
                    form,
                }],
                extra,
                false,
            ));
        }
    }
    Some((vec![MInst::CmpRR { ty, lhs: a, rhs: b }], Vec::new(), false))
}

/// Mirror a condition for swapped operands: `a < b` becomes `b > a`.
///
/// Not the same as [`CmpOp::negate`] — negation is for inverting a *branch*,
/// this is for exchanging the operands, and confusing the two inverts the
/// program.
fn mirror(cc: CmpOp) -> CmpOp {
    match cc {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

/// `Op::Cmp` whose 0/1 value is actually read: `CMP; SETcc; MOVZX`.
fn tile_cmp_setcc(ctx: &SelCtx, root: NodeId, claimed: &[bool]) -> Option<Tile> {
    let cc = match ctx.graph.node_opt(root)?.op {
        IrOp::Cmp(cc) => cc,
        _ => return None,
    };
    let (mut insts, extra, swapped) = compare_insts(ctx, root, claimed)?;
    let cc = if swapped { mirror(cc) } else { cc };
    insts.push(MInst::SetCc { dst: root, cc });
    let mut covered = vec![root];
    covered.extend(extra);
    Some(Tile::new(root, covered, insts, Rule::CmpSetCc))
}

/// The terminator's candidate tiles, **most preferred first**.
///
/// Two candidates at most: the fused `CMP; Jcc` (or `TEST; Jcc`) when the
/// compare exists only to feed this branch, and the unfused `TEST cond, cond;
/// JNE` that `ir_lower::lower_terminator` already emits. The list is ordered
/// rather than costed because the second entry is a *fall-back*, not a rival:
/// the caller takes the first one the encodability gate admits, so a fused
/// form the table cannot encode degrades to the form it can instead of
/// degrading to nothing.
fn tile_terminator(ctx: &SelCtx, term: NodeId, claimed: &[bool]) -> Vec<Tile> {
    let mut out = Vec::new();
    let node = match ctx.graph.node_opt(term) {
        Some(n) => n,
        None => return out,
    };
    if !matches!(node.op, IrOp::If) {
        return out;
    }
    let cond = match node.input_opt(1) {
        Some(c) => c,
        None => return out,
    };
    // Fuse only when the compare exists solely to feed this branch. A compare
    // with a second consumer still has to materialise its 0/1 value, and
    // fusing would delete it. It must also be in *this* block: a compare the
    // scheduler placed in a dominator computes flags that any instruction in
    // between would have destroyed.
    let cmp_cc = match ctx.graph.node_opt(cond).map(|n| &n.op) {
        Some(&IrOp::Cmp(cc)) if ctx.in_block(cond) && ctx.uses.single_use(cond) => Some(cc),
        _ => None,
    };
    if let Some(cc) = cmp_cc {
        if let Some((mut insts, extra, swapped)) = compare_insts(ctx, cond, claimed) {
            let cc = if swapped { mirror(cc) } else { cc };
            let is_test = insts.iter().any(|i| matches!(i, MInst::TestRR { .. }));
            insts.push(MInst::Jcc { cc, at: term });
            let mut covered = vec![term, cond];
            covered.extend(extra);
            let rule = if is_test {
                Rule::TestZeroBranch
            } else {
                Rule::CmpBranch
            };
            out.push(Tile::new(term, covered, insts, rule));
        }
    }
    // The fall-back: `TEST cond, cond; JNE`, which is byte-for-byte what
    // `ir_lower::lower_terminator` already emits.
    let ty = ctx.int_ty(cond).unwrap_or(Ty::I32);
    out.push(Tile::new(
        term,
        vec![term],
        vec![
            MInst::TestRR { ty, reg: cond },
            MInst::Jcc {
                cc: CmpOp::Ne,
                at: term,
            },
        ],
        Rule::TestBranch,
    ));
    out
}

// ── The driver ───────────────────────────────────────────────────────────

/// Select instructions for one scheduled basic block.
///
/// `block` is the block's data nodes in the order `ir_lower` will emit them
/// (`ir_schedule::Block::nodes`), and `terminator` its `Op::If` / `Op::Return`
/// (`ir_schedule::Block::terminator`).
///
/// # Algorithm
///
/// Maximal munch in two passes, because a tile that absorbs a node has to claim
/// it *before* the node's own position is reached:
///
/// 1. **Claim, in reverse order.** The terminator goes first (it is the last
///    thing in the block and it is the one that can fuse a compare), then each
///    node from the end backwards. Each unclaimed node offers its candidate
///    tiles; the cheapest by [`Tile::net_key`] — cost minus what it displaces
///    — wins and claims its interior nodes. Reverse order is what gives the
///    *consumer* first refusal on its operands, which is the direction folding
///    moves in.
/// 2. **Emit, in forward order.** Every node that is still a root emits its
///    tile, in the block's own order, and the terminator's tile goes last.
///
/// # Guarantees
///
/// * Total — never panics, never fails. A malformed node becomes a
///   [`Rule::Generic`] tile.
/// * Every node in `block` is covered exactly once
///   ([`BlockSelection::covers`]).
/// * Under the default [`SelectOptions`], every non-generic tile has a proven
///   encoding.
pub fn select_block(
    graph: &Graph,
    block: &[NodeId],
    terminator: Option<NodeId>,
    opts: &SelectOptions,
) -> BlockSelection {
    MethodSelector::new(graph).select_block(block, terminator, opts)
}

/// Reuse the method-wide use counts and per-node scratch across blocks.
pub(crate) struct MethodSelector<'g> {
    ctx: SelCtx<'g>,
    claimed: Vec<bool>,
    chosen: Vec<Option<Tile>>,
}

impl<'g> MethodSelector<'g> {
    pub(crate) fn new(graph: &'g Graph) -> Self {
        let n = graph.nodes.len();
        Self {
            ctx: SelCtx::new(graph, &[]),
            claimed: vec![false; n],
            chosen: (0..n).map(|_| None).collect(),
        }
    }

    pub(crate) fn allow_checked_array_reads(&mut self, checks: &crate::ir_check_elim::CheckElision) {
        self.ctx.checked_array_reads = Some(
            self.ctx
                .graph
                .nodes
                .iter()
                .enumerate()
                .map(|(id, _)| checked_integer_array_address(self.ctx.graph, checks, id as NodeId))
                .collect(),
        );
    }

    pub(crate) fn protect_values(&mut self, values: &[NodeId]) {
        for &id in values {
            if let Some(pin) = self.ctx.uses.pinned.get_mut(id as usize) {
                *pin = true;
            }
        }
    }

    pub(crate) fn select_block(
        &mut self,
        block: &[NodeId],
        terminator: Option<NodeId>,
        opts: &SelectOptions,
    ) -> BlockSelection {
        // Only nodes in the previous block can have been absorbed. The forward
        // pass has already taken all its chosen tiles.
        for &id in self.ctx.order() {
            if let Some(slot) = self.claimed.get_mut(id as usize) {
                *slot = false;
            }
        }
        self.ctx.set_block(block);
        select_block_with(
            &self.ctx,
            terminator,
            opts,
            &mut self.claimed,
            &mut self.chosen,
        )
    }
}

fn select_block_with(
    ctx: &SelCtx<'_>,
    terminator: Option<NodeId>,
    opts: &SelectOptions,
    claimed: &mut [bool],
    chosen: &mut [Option<Tile>],
) -> BlockSelection {
    let block = ctx.order();
    let mut notes: Vec<Note> = Vec::new();

    // Pass 1a: the terminator.
    //
    // A terminator ALWAYS gets a tile. A branch that selection declined to
    // cover is a branch nobody emits, which is the one failure this design
    // must not have: when the fused form is refused, the fall-back is the
    // unfused `TEST; Jcc`, and when that is refused too it is a
    // `Rule::Generic` tile naming the `Op::If`.
    let term_tile = terminator.map(|t| {
        let tile = tile_terminator(ctx, t, claimed)
            .into_iter()
            .find_map(|tile| admit(tile, opts, &mut notes))
            .unwrap_or_else(|| Tile::generic(t));
        mark_claims(&tile, claimed);
        tile
    });

    // Pass 1b: the data nodes, back to front.
    for &id in block.iter().rev() {
        if claimed.get(id as usize).copied().unwrap_or(false) {
            continue;
        }
        let mut cands: Vec<Tile> = Vec::new();
        // ALU forms are offered BEFORE `LEA`, and the order is load-bearing:
        // `min_by_key` keeps the first minimum, so this is the tie-break. On a
        // one-register address (`p + 24`) the two are identical on every axis
        // the cost model measures — 1 uop, 4 bytes, latency 1 — and the ALU
        // form is still the better answer, because `LEA` occupies the address
        // generation unit and carries worse latency on several
        // microarchitectures than the table's uniform figure admits. Offering
        // `LEA` first made `p + 24` select an `LEA`, which is what
        // `a_constant_that_fits_imm8_becomes_an_immediate` caught the first
        // time this file was ever compiled.
        //
        // Ties are the only thing this order decides. Where `LEA` genuinely
        // wins — a live left operand, where the ALU form must copy first — it
        // wins on cost regardless of position; see
        // `lea_wins_only_when_the_alu_form_would_need_a_copy`.
        cands.extend(tiles_alu(ctx, id, claimed, opts, &mut notes));
        if opts.allocated_division {
            cands.extend(tile_integer_division(ctx, id));
        }
        if let Some(t) = tile_lea(ctx, id, claimed, &mut notes) {
            cands.push(t);
        }
        if let Some(t) = tile_cmp_setcc(ctx, id, claimed) {
            cands.push(t);
        }
        let best = cands
            .into_iter()
            .filter_map(|t| admit(t, opts, &mut notes))
            .min_by_key(|t| t.net_key())
            .unwrap_or_else(|| Tile::generic(id));
        mark_claims(&best, claimed);
        if let Some(slot) = chosen.get_mut(id as usize) {
            *slot = Some(best);
        }
    }

    // Pass 2: emit in block order.
    let mut tiles = Vec::with_capacity(block.len());
    for &id in block {
        let taken = chosen.get_mut(id as usize).and_then(|s| s.take());
        if let Some(t) = taken {
            tiles.push(t);
        }
    }
    if let Some(t) = term_tile {
        tiles.push(t);
    }
    let cost = tiles
        .iter()
        .fold(SeqCost::default(), |acc, t| acc.then(t.cost));
    BlockSelection { tiles, cost, notes }
}

/// Mark every node a tile absorbed (its root excepted) as claimed.
fn mark_claims(t: &Tile, claimed: &mut [bool]) {
    for &c in &t.covered {
        if c == t.root {
            continue;
        }
        if let Some(slot) = claimed.get_mut(c as usize) {
            *slot = true;
        }
    }
}

/// Admit a tile, or record why the table cannot encode it.
///
/// The fail-closed step: under [`SelectOptions::require_encodable`] a tile the
/// pattern table cannot produce bytes for is **discarded**, so the node falls
/// back to the generic lowering instead of being selected into an instruction
/// nobody can emit.
fn admit(t: Tile, opts: &SelectOptions, notes: &mut Vec<Note>) -> Option<Tile> {
    // The allocation the consumer will encode against changes what a tile
    // costs, and it changes it differently for different candidates. Applied
    // here rather than in each rule so that every candidate for a node is
    // priced the same way — a rule that forgot would look cheap.
    let t = if opts.frame_homed { t.frame_homed() } else { t };
    if !opts.require_encodable || (opts.allocated_setcc && t.rule == Rule::CmpSetCc) {
        return Some(t);
    }
    match t.encodable() {
        Ok(()) => Some(t),
        Err(why) => {
            notes.push(Note::Unencodable {
                root: t.root,
                rule: t.rule,
                why,
            });
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Shadow selection — increment 0 of
// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`
// ---------------------------------------------------------------------------
//
// Run the tiler over a real compile's blocks, check its own invariant, count
// what fired, and **throw the result away**. Not one emitted byte changes.
//
// Why a pass that emits nothing is the first increment, and not a shortcut to
// one that does: the contract's three-defect test scored one of three, so the
// HIR/MIR migration is not justified as a correctness investment. What decides
// whether to continue is a number nobody has — how much of a real method the
// pattern table can actually cover — and this is the cheapest honest way to get
// it. If the answer is small, the right move is to stop and keep the number.
//
// Measured on ten synthetic shapes for the contract: 38.2% of scheduled data
// nodes, with `Rule::AluImm` firing **zero** times because the table has no
// 32-bit immediate rows. That corpus is not real code — it has no field access,
// no calls, and `ir_optimize` never ran on it. This pass replaces it with the
// real population.
//
// ## Why this does not refuse the compile
//
// Everything else new in this backend fails closed. This deliberately does not,
// and the reason is that it is a *measurement*: a flag whose only documented
// effect is a count must not be able to change which methods get compiled, or
// the number it reports is a number about a different program. So a
// `covers()` violation — an `isel` bug, and the exact failure the invariant
// exists to catch — is counted and reported, loudly, and the compile proceeds
// through the unchanged path.
//
// `shadow_selection_changes_no_emitted_byte` is what holds that claim up.
// Increment 1, which emits tiles, is where fail-closed comes back.

/// One method's shadow-selection result.
///
/// Counts only. Nothing here names a node, because nothing downstream may act
/// on it — see the module note above.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowStats {
    /// Blocks the selector was run over.
    pub blocks: u64,
    /// Scheduled data nodes offered to it (`Block::nodes`, summed).
    pub nodes: u64,
    /// Tiles it produced, including `Rule::Generic` ones.
    pub tiles: u64,
    /// Tiles that fired a rule other than [`Rule::Generic`].
    pub matched_tiles: u64,
    /// Data nodes covered by a matched tile — the headline figure. A tile's
    /// `covered` list can hold more than its root (that is what absorption is),
    /// so this is not `matched_tiles`.
    pub covered_nodes: u64,
    /// Blocks where [`BlockSelection::covers`] came back false.
    ///
    /// **Always zero, or there is a bug in `isel`.** Counted rather than
    /// asserted because this pass may not change what compiles.
    pub coverage_failures: u64,
    /// Per-rule tile counts, indexed by [`rule_index`].
    pub rules: [u64; RULE_COUNT],
    /// Per-note refusal counts, indexed by [`note_index`].
    pub notes: [u64; NOTE_COUNT],
}

/// Number of [`Rule`] variants. A new variant is a compile error in
/// [`rule_index`], which is the point.
pub const RULE_COUNT: usize = 10;
/// Number of [`Note`] variants; same discipline as [`RULE_COUNT`].
pub const NOTE_COUNT: usize = 4;

/// Stable histogram slot for a rule. **Exhaustive on purpose** — adding a
/// [`Rule`] variant must not silently land in another variant's bucket.
pub fn rule_index(rule: Rule) -> usize {
    match rule {
        Rule::Generic => 0,
        Rule::Lea => 1,
        Rule::AluImm => 2,
        Rule::AluReg => 3,
        Rule::AluFoldedLoad => 4,
        Rule::CmpBranch => 5,
        Rule::TestZeroBranch => 6,
        Rule::TestBranch => 7,
        Rule::CmpSetCc => 8,
        Rule::GuardedDiv => 9,
    }
}

/// Human name for histogram slot `i`, parallel to [`rule_index`].
pub fn rule_name(i: usize) -> &'static str {
    [
        "Generic",
        "Lea",
        "AluImm",
        "AluReg",
        "AluFoldedLoad",
        "CmpBranch",
        "TestZeroBranch",
        "TestBranch",
        "CmpSetCc",
        "GuardedDiv",
    ]
    .get(i)
    .copied()
    .unwrap_or("?")
}

/// Stable histogram slot for a refusal note. Exhaustive, as [`rule_index`].
pub fn note_index(note: &Note) -> usize {
    match note {
        Note::Address { .. } => 0,
        Note::Fold { .. } => 1,
        Note::Unencodable { .. } => 2,
        Note::WideImmediate { .. } => 3,
    }
}

/// Human name for note slot `i`, parallel to [`note_index`].
pub fn note_name(i: usize) -> &'static str {
    ["Address", "Fold", "Unencodable", "WideImmediate"]
        .get(i)
        .copied()
        .unwrap_or("?")
}

impl ShadowStats {
    /// Fraction of scheduled data nodes a real rule covered, as a percentage.
    ///
    /// This is the figure increment 0 exists to produce. Zero nodes reads as
    /// `0.0` rather than NaN: a method with nothing to select is not 100%
    /// covered, and a NaN in a summary line is how a metric gets ignored.
    pub fn coverage_pct(&self) -> f64 {
        if self.nodes == 0 {
            return 0.0;
        }
        100.0 * self.covered_nodes as f64 / self.nodes as f64
    }

    /// Fold `other` into `self`. Used by callers aggregating several methods
    /// without going through the process totals (the tests do this).
    pub fn add(&mut self, other: &ShadowStats) {
        self.blocks += other.blocks;
        self.nodes += other.nodes;
        self.tiles += other.tiles;
        self.matched_tiles += other.matched_tiles;
        self.covered_nodes += other.covered_nodes;
        self.coverage_failures += other.coverage_failures;
        for i in 0..RULE_COUNT {
            self.rules[i] += other.rules[i];
        }
        for i in 0..NOTE_COUNT {
            self.notes[i] += other.notes[i];
        }
    }

    /// One line, in the shape the aggregation script reads.
    pub fn summary_line(&self) -> String {
        let mut s = format!(
            "blocks={} nodes={} tiles={} matched={} covered={} ({:.1}%) covfail={}",
            self.blocks,
            self.nodes,
            self.tiles,
            self.matched_tiles,
            self.covered_nodes,
            self.coverage_pct(),
            self.coverage_failures,
        );
        for i in 0..RULE_COUNT {
            if self.rules[i] != 0 {
                s.push_str(&format!(" {}={}", rule_name(i), self.rules[i]));
            }
        }
        for i in 0..NOTE_COUNT {
            if self.notes[i] != 0 {
                s.push_str(&format!(" note:{}={}", note_name(i), self.notes[i]));
            }
        }
        s
    }
}

// The process-wide accumulator.
//
// Plain atomics rather than a `Mutex<ShadowStats>`: this runs inside the
// compiler, on whatever thread the broker picked, and a compile must never
// block on a diagnostic. Relaxed ordering makes the totals a *sample* — the
// same contract `bailout::bailout_counts` documents — which is what a coverage
// figure needs and all it needs.
mod totals {
    use super::{NOTE_COUNT, RULE_COUNT};
    use std::sync::atomic::AtomicU64;

    pub(super) static BLOCKS: AtomicU64 = AtomicU64::new(0);
    pub(super) static NODES: AtomicU64 = AtomicU64::new(0);
    pub(super) static TILES: AtomicU64 = AtomicU64::new(0);
    pub(super) static MATCHED: AtomicU64 = AtomicU64::new(0);
    pub(super) static COVERED: AtomicU64 = AtomicU64::new(0);
    pub(super) static COVFAIL: AtomicU64 = AtomicU64::new(0);
    pub(super) static METHODS: AtomicU64 = AtomicU64::new(0);
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    pub(super) static RULES: [AtomicU64; RULE_COUNT] = [ZERO; RULE_COUNT];
    pub(super) static NOTES: [AtomicU64; NOTE_COUNT] = [ZERO; NOTE_COUNT];
}

/// Methods this process has shadow-selected, and their summed stats.
pub fn shadow_totals() -> (u64, ShadowStats) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut s = ShadowStats {
        blocks: totals::BLOCKS.load(Relaxed),
        nodes: totals::NODES.load(Relaxed),
        tiles: totals::TILES.load(Relaxed),
        matched_tiles: totals::MATCHED.load(Relaxed),
        covered_nodes: totals::COVERED.load(Relaxed),
        coverage_failures: totals::COVFAIL.load(Relaxed),
        ..ShadowStats::default()
    };
    for i in 0..RULE_COUNT {
        s.rules[i] = totals::RULES[i].load(Relaxed);
    }
    for i in 0..NOTE_COUNT {
        s.notes[i] = totals::NOTES[i].load(Relaxed);
    }
    (totals::METHODS.load(Relaxed), s)
}

/// **Test support.** Serialises every test that touches the shadow counters.
///
/// The accumulator is process-global and the test binary is threaded, so a test
/// that resets it and then asserts `methods == 1` will read another test's
/// compile if the two overlap — which is exactly what happened the first time
/// these were run (`left: 2, right: 1`). Any test that enables
/// `CRATONVM_JIT_IR_ISEL_SHADOW` or reads [`shadow_totals`] must hold this,
/// including one that only asserts the counters stayed at zero.
///
/// Deliberately its own lock and not `metrics::METRICS_TEST_LOCK`: these tests
/// have nothing to do with metrics, and a reader should not have to work out
/// why a byte-comparison test takes a metrics lock.
#[cfg(test)]
pub static SHADOW_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Zero the accumulator. Tests only — two tests reading one global would
/// otherwise see each other's counts. Hold [`SHADOW_TEST_LOCK`] across the
/// reset AND the read.
#[cfg(test)]
pub fn reset_shadow_totals() {
    use std::sync::atomic::Ordering::Relaxed;
    for c in [
        &totals::BLOCKS,
        &totals::NODES,
        &totals::TILES,
        &totals::MATCHED,
        &totals::COVERED,
        &totals::COVFAIL,
        &totals::METHODS,
    ] {
        c.store(0, Relaxed);
    }
    for c in totals::RULES.iter().chain(totals::NOTES.iter()) {
        c.store(0, Relaxed);
    }
}

/// Tile one method's blocks, count, discard.
///
/// Returns this method's stats and folds them into the process totals. The
/// caller emits through the unchanged path either way; nothing in the return
/// value may reach the emitter.
///
/// `SelectOptions::default()` is deliberate and not a placeholder: it is
/// `require_encodable: true` (refuse a tile the table cannot encode) and
/// `fold_loads: false` (the load-fold gate is sound but its address half is
/// still `AddrSource::Opaque`). Measuring with `fold_loads: true` would report
/// coverage no production wiring could take.
pub fn shadow_select_method(graph: &Graph, schedule: &Schedule) -> ShadowStats {
    use std::sync::atomic::Ordering::Relaxed;

    let opts = SelectOptions::default();
    let mut m = ShadowStats::default();
    let mut selector = MethodSelector::new(graph);
    for block in &schedule.blocks {
        let sel = selector.select_block(&block.nodes, block.terminator, &opts);
        m.blocks += 1;
        m.nodes += block.nodes.len() as u64;
        m.tiles += sel.tiles.len() as u64;
        if !sel.covers(&block.nodes) {
            m.coverage_failures += 1;
        }
        for t in &sel.tiles {
            m.rules[rule_index(t.rule)] += 1;
            if t.rule != Rule::Generic {
                m.matched_tiles += 1;
                m.covered_nodes += t.covered.len() as u64;
            }
        }
        for n in &sel.notes {
            m.notes[note_index(n)] += 1;
        }
    }

    totals::METHODS.fetch_add(1, Relaxed);
    totals::BLOCKS.fetch_add(m.blocks, Relaxed);
    totals::NODES.fetch_add(m.nodes, Relaxed);
    totals::TILES.fetch_add(m.tiles, Relaxed);
    totals::MATCHED.fetch_add(m.matched_tiles, Relaxed);
    totals::COVERED.fetch_add(m.covered_nodes, Relaxed);
    totals::COVFAIL.fetch_add(m.coverage_failures, Relaxed);
    for i in 0..RULE_COUNT {
        totals::RULES[i].fetch_add(m.rules[i], Relaxed);
    }
    for i in 0..NOTE_COUNT {
        totals::NOTES[i].fetch_add(m.notes[i], Relaxed);
    }
    m
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "isel/allocated_tests.rs"]
mod allocated_tests;

#[cfg(test)]
mod checked_array_selection_tests;
