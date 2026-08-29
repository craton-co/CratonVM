// SPDX-License-Identifier: Apache-2.0
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
//! (`mov_r64_imm0_xor`, `mov_r64_imm32`, `mov_r64_imm64`) rather than as `if`
//! statements, the selector reproduces `emit_mov_imm64`'s three-way shrink by
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
//! Nothing in `x64.rs` calls this module yet. This pass builds the table and
//! proves the equivalence; migrating call sites onto it is deliberately a
//! later, separate change (see `docs/jit/instruction-patterns.md` for the
//! migration order).
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
//! `#[cfg(test)]`. Every fallible operation returns [`SelError`].

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
/// loads it names the *source* width (the destination of a `Movsx`/`Movzx`
/// row is always 64-bit).
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
        }
    }

    /// The same request with the `extra` slot filled in.
    pub fn with_extra(self, extra: Operand) -> Req {
        Req { extra, ..self }
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
    /// Eight bytes, little-endian.
    Imm64,
}

impl ImmForm {
    /// Number of bytes this immediate occupies.
    pub fn byte_len(self) -> usize {
        match self {
            ImmForm::None => 0,
            ImmForm::Imm8 | ImmForm::ImmU8 => 1,
            ImmForm::Imm32 => 4,
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
        match self.enc.rex {
            RexMode::Never => {}
            RexMode::OnDemand => {
                // 0x40 carries no information; the hand-written emitters omit
                // it and so must the table.
                if rex_byte != 0x40 {
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
                out.bytes.push(v);
            }
            ImmForm::Imm32 => {
                let v = i32::try_from(a.imm).map_err(|_| SelError::Immediate {
                    pattern: self.name,
                    form: ImmForm::Imm32,
                    value: a.imm,
                })?;
                out.bytes.extend_from_slice(&v.to_le_bytes());
            }
            ImmForm::Imm64 => out.bytes.extend_from_slice(&a.imm.to_le_bytes()),
        }

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
    Pattern {
        name: "and_r64_r64",
        emitter: "ir_lower.rs inline `[0x48, 0x21, 0xC8]` (Op::And)",
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
// # Not wired
//
// Nothing calls [`select_block`] in the production pipeline. It is a component
// with its own gate, exactly like the pattern table above it; see
// `docs/jit/instruction-selection.md` for what the production wiring has to do
// and what is still unvalidated.

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
            for &id in sp.locals.iter().chain(sp.stack.iter()) {
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
        let mut pos = vec![usize::MAX; graph.nodes.len()];
        for (i, &id) in block.iter().enumerate() {
            if let Some(slot) = pos.get_mut(id as usize) {
                // A duplicated entry keeps its FIRST position: the earlier one
                // is the conservative answer for every "is this before that"
                // question below.
                if *slot == usize::MAX {
                    *slot = i;
                }
            }
        }
        SelCtx {
            graph,
            uses: ValueUses::of(graph),
            pos,
            order: block.to_vec(),
        }
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
                // A constant only folds into the displacement when nothing
                // else reads it; otherwise it keeps its register and becomes a
                // term. (`absorbable` also refuses a constant from another
                // block, which is the loop-invariant case.)
                if ctx.absorbable(id) {
                    acc.absorbed.push(id);
                    let v = *c;
                    acc.disp(v)?;
                } else {
                    acc.plain(id)?;
                }
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

impl MInst {
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
                v if i32::try_from(v).is_ok() => "mov_r64_imm32",
                _ => "mov_r64_imm64",
            }),
            MInst::Imm { .. } => None,
            MInst::Move { ty: Ty::I64, .. } => Some("mov_r64_r64"),
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
                (Op::Or, Ty::I64) => Some("or_r64_r64"),
                (Op::Xor, Ty::I64) => Some("xor_r64_r64"),
                (Op::Imul, Ty::I64) => Some("imul_r64_r64"),
                (Op::Imul, Ty::I32) => Some("imul_r32_r32"),
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
                _ => None,
            },
            MInst::AluRI { .. } => None,
            MInst::AluRM { .. } => None,
            MInst::CmpRR { ty: Ty::I64, .. } => Some("cmp_r64_r64"),
            MInst::CmpRR { ty: Ty::I32, .. } => Some("cmp_r32_r32"),
            MInst::CmpRR { .. } => None,
            MInst::CmpRI {
                ty: Ty::I32,
                form: ImmForm::Imm8,
                ..
            } => Some("cmp_r32_imm8"),
            MInst::CmpRI { .. } => None,
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

    /// Build a tile directly. Tests only.
    ///
    /// In production only the rules construct tiles, so a tile's cover list and
    /// its instructions always come from one place. A test that needs to hand a
    /// *deliberately inconsistent* tile to a consumer — `ir_lower::
    /// mir_tile_is_emittable`'s absorbed-node guard is the one that does —
    /// cannot obtain one from a rule by construction, which is exactly why that
    /// guard needs this.
    #[cfg(test)]
    pub fn for_test(root: NodeId, covered: Vec<NodeId>, insts: Vec<MInst>, rule: Rule) -> Tile {
        Tile::new(root, covered, insts, rule)
    }

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
}

impl Default for SelectOptions {
    fn default() -> SelectOptions {
        SelectOptions {
            require_encodable: true,
            fold_loads: false,
            frame_homed: false,
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
/// `And` / `Or` / `Xor` come back as [`Ty::I64`] for an `Int` node too, which
/// is what `ir_lower` already does (`[0x48, 0x21, 0xC8]` for every `Op::And`):
/// the low 32 bits of a 64-bit bitwise operation are the 32-bit result, so the
/// wide form is correct for both and there is only one row to keep honest.
fn alu_op_of(op: &IrOp, ty: Ty) -> Option<(Op, Ty)> {
    Some(match op {
        IrOp::Add => (Op::Add, ty),
        IrOp::Sub => (Op::Sub, ty),
        IrOp::Mul => (Op::Imul, ty),
        IrOp::And => (Op::And, Ty::I64),
        IrOp::Or => (Op::Or, Ty::I64),
        IrOp::Xor => (Op::Xor, Ty::I64),
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
    if let Some((reg, kn, c)) = const_side {
        match imm_form_for(c) {
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

    // ── a load folded into the memory operand ────────────────────────────
    if opts.fold_loads {
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
                    let mut insts = prefix(keep);
                    insts.push(MInst::AluRM {
                        op,
                        ty: opty,
                        dst: root,
                        lhs: keep,
                        addr: AddrSource::Opaque(fold),
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
    let ctx = SelCtx::new(graph, block);
    let n = graph.nodes.len();
    let mut claimed = vec![false; n];
    let mut chosen: Vec<Option<Tile>> = (0..n).map(|_| None).collect();
    let mut notes: Vec<Note> = Vec::new();

    // Pass 1a: the terminator.
    //
    // A terminator ALWAYS gets a tile. A branch that selection declined to
    // cover is a branch nobody emits, which is the one failure this design
    // must not have: when the fused form is refused, the fall-back is the
    // unfused `TEST; Jcc`, and when that is refused too it is a
    // `Rule::Generic` tile naming the `Op::If`.
    let term_tile = terminator.map(|t| {
        let tile = tile_terminator(&ctx, t, &claimed)
            .into_iter()
            .find_map(|tile| admit(tile, opts, &mut notes))
            .unwrap_or_else(|| Tile::generic(t));
        mark_claims(&tile, &mut claimed);
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
        cands.extend(tiles_alu(&ctx, id, &claimed, opts, &mut notes));
        if let Some(t) = tile_lea(&ctx, id, &claimed, &mut notes) {
            cands.push(t);
        }
        if let Some(t) = tile_cmp_setcc(&ctx, id, &claimed) {
            cands.push(t);
        }
        let best = cands
            .into_iter()
            .filter_map(|t| admit(t, opts, &mut notes))
            .min_by_key(|t| t.net_key())
            .unwrap_or_else(|| Tile::generic(id));
        mark_claims(&best, &mut claimed);
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
    if !opts.require_encodable {
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
pub const RULE_COUNT: usize = 9;
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
    for block in &schedule.blocks {
        let sel = select_block(graph, &block.nodes, block.terminator, &opts);
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
mod tests {
    use super::*;
    use crate::x64::{R11, R12, R13, R15, R8, R9, RAX, RBP, RBX, RCX, RDI, RDX, RSI, RSP};

    // ── reference-emitter harness ─────────────────────────────────────────

    /// A live `Compiler` whose hand-written emitters are the ground truth.
    ///
    /// Every equivalence test drives the real emitter and the pattern table
    /// with the same operands and compares the bytes. Nothing here
    /// reimplements an encoding: the reference side is `x64.rs` itself.
    struct Reference {
        compiler: super::super::Compiler,
    }

    impl Reference {
        fn new() -> Reference {
            let alloc = crate::regalloc::RegAllocResult {
                assignments: Vec::new(),
                xmm_assignments: Vec::new(),
                used_callee_saved: Vec::new(),
                used_xmm_regs: Vec::new(),
                block_live_in: Vec::new(),
            };
            let compiler = super::super::Compiler::new(
                "isel-equivalence".to_string(),
                crate::ExecutableBuffer::new(1 << 18).expect("test executable buffer"),
                0,
                0,
                1,
                false,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                alloc,
                false,
                cratonvm_jit_api::JitRuntimeHelpers::default(),
                0,
                false,
                false,
                false,
                false,
                // No `invokedynamic` in this fixture — see the sibling call
                // site in `x64/tests.rs`.
                false,
                Vec::new(),
            );
            Reference { compiler }
        }

        /// Run one hand-written emitter and return exactly the bytes it added.
        fn emit<F>(&mut self, f: F) -> Vec<u8>
        where
            F: FnOnce(&mut super::super::Compiler),
        {
            let start = self.compiler.buf.pos();
            // `emit_load_local` elides a reload when the previous emission
            // stored the same slot; clearing the mirror keeps each sample in
            // the sweep independent of its predecessor.
            self.compiler.slot_mirror = None;
            f(&mut self.compiler);
            assert!(
                !self.compiler.buf.overflowed(),
                "the reference emitter overflowed the test buffer"
            );
            self.compiler.buf.as_slice()[start..].to_vec()
        }
    }

    /// Register sweep: both halves of the file, both SIB special cases
    /// (RSP/R12) and both no-`mod=00` special cases (RBP/R13).
    const REGS: [u8; 13] = [
        RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8, R11, R12, R13, R15,
    ];

    /// Bases that a no-SIB encoding can legally address.
    fn non_sib_regs() -> Vec<u8> {
        REGS.iter().copied().filter(|r| r & 7 != 0b100).collect()
    }

    fn sel(req: &Req) -> Vec<u8> {
        match select(req) {
            Ok(s) => s.encoded.bytes,
            Err(e) => panic!("no pattern for {req:?}: {e}"),
        }
    }

    fn named(name: &str, a: &Args) -> Vec<u8> {
        match encode_named(name, a) {
            Ok(e) => e.bytes,
            Err(e) => panic!("pattern `{name}` refused {a:?}: {e}"),
        }
    }

    fn gpr(op: Op, ty: Ty, dst: u8, src: u8) -> Req {
        Req::new(op, ty, Operand::Gpr(dst), Operand::Gpr(src))
    }

    fn gpr_imm(op: Op, ty: Ty, dst: u8, imm: i64) -> Req {
        Req::new(op, ty, Operand::Gpr(dst), Operand::Imm(imm))
    }

    /// A condition/opcode byte that satisfies the row's own constraints AND
    /// cannot be mistaken for a REX prefix by the structural decoder — a
    /// parametric opcode in `0x40..=0x4F` would decode as one.
    fn canonical_op_byte(p: &Pattern) -> u8 {
        match p.op {
            Op::Jcc => 0x85,  // JNE rel32
            Op::Cmov => 0x45, // CMOVNE
            _ => 0x31,        // XOR r/m32, r32 — a real one-byte ALU opcode
        }
    }

    /// The smallest immediate each row's declared field and constraints admit.
    fn canonical_imm(p: &Pattern) -> i64 {
        match p.imm {
            // The wide-move rows require a value that does NOT fit i32.
            ImmForm::Imm64 => i64::from(i32::MAX) + 1,
            ImmForm::ImmU8 => 1,
            _ => 0,
        }
    }

    /// Operands that satisfy every constraint the row declares, so a
    /// table-wide sweep exercises each row instead of skipping it.
    fn args_for(p: &Pattern, dst: u8, src: u8, base: u8, index: u8, disp: i64) -> Args {
        Args {
            dst,
            src: if p.constraints.contains(&Constraint::SameRegister) {
                dst
            } else {
                src
            },
            mem: if p.constraints.contains(&Constraint::IndexNotRsp) {
                Mem::base_index(base, index, 4, disp)
            } else if matches!(p.disp, DispPolicy::Force32) {
                Mem::base_disp32(base, disp)
            } else {
                Mem::base_disp(base, disp)
            },
            imm: canonical_imm(p),
            op_byte: canonical_op_byte(p),
        }
    }

    // ── table well-formedness ─────────────────────────────────────────────

    #[test]
    fn every_pattern_name_is_unique_and_names_its_emitter() {
        for (i, p) in PATTERNS.iter().enumerate() {
            assert!(!p.name.is_empty(), "row {i} has no name");
            assert!(
                !p.emitter.is_empty(),
                "row `{}` names no reference emitter; every row must be \
                 anchored to the hand-written code it reproduces",
                p.name
            );
            for q in PATTERNS.iter().skip(i + 1) {
                assert_ne!(p.name, q.name, "duplicate pattern name `{}`", p.name);
            }
        }
    }

    /// Schema invariants that, if broken, silently change what a row encodes.
    #[test]
    fn encoding_templates_are_internally_consistent() {
        for p in PATTERNS {
            if let RegF::Ext(n) = p.enc.reg {
                assert!(n < 8, "`{}`: opcode extension /{n} is not 0..=7", p.name);
            }
            if p.enc.rex_w {
                assert_eq!(
                    p.enc.rex,
                    RexMode::Always,
                    "`{}`: REX.W is part of the encoding, so the REX byte cannot be optional",
                    p.name
                );
            }
            if matches!(p.enc.rex, RexMode::Never) {
                assert!(
                    !p.enc.rex_w,
                    "`{}`: cannot suppress REX and request REX.W",
                    p.name
                );
            }
            assert_eq!(
                p.is_memory(),
                !matches!(p.disp, DispPolicy::NotApplicable),
                "`{}`: a displacement policy and a memory operand must agree",
                p.name
            );
            if p.is_memory() {
                assert!(
                    matches!(p.dst, OpKind::Mem) || matches!(p.src, OpKind::Mem),
                    "`{}`: addresses memory but declares no memory operand",
                    p.name
                );
            }
            if !p.enc.modrm {
                assert_eq!(
                    p.enc.rm,
                    RmF::None,
                    "`{}`: no ModRM byte, so no r/m field",
                    p.name
                );
            }
        }
    }

    /// `PlusReg` folds the register into the opcode's low three bits, so
    /// those bits must be clear in the base byte or the fold corrupts it.
    #[test]
    fn plus_reg_opcodes_have_room_for_the_register() {
        for p in PATTERNS {
            if let Opcode::PlusReg(b) = p.enc.opcode {
                assert_eq!(
                    b & 7,
                    0,
                    "`{}`: opcode {b:#04X} has no register room",
                    p.name
                );
                for r in 0u8..8 {
                    assert_eq!(b | r, b + r, "`{}`: fold must be an OR", p.name);
                }
            }
        }
    }

    /// `cost.bytes` is a floor, not a guess: nothing may encode shorter.
    ///
    /// This also proves every row can encode *something*: a row whose
    /// constraints contradict its own encoding template would fail here
    /// rather than sit in the table looking authoritative.
    #[test]
    fn declared_cost_is_a_lower_bound_on_the_encoded_length() {
        for p in PATTERNS {
            // Zero displacement, low registers: the shortest operands any
            // row admits.
            let a = args_for(p, RAX, RCX, RAX, RCX, 0);
            let e = match p.encode(&a) {
                Ok(e) => e,
                Err(err) => {
                    panic!(
                        "`{}` could not encode its canonical operands: {err}",
                        p.name
                    )
                }
            };
            assert!(
                e.bytes.len() >= usize::from(p.cost.bytes),
                "`{}`: cost says {} bytes but it encoded {}",
                p.name,
                p.cost.bytes,
                e.bytes.len()
            );
        }
    }

    // ── byte-for-byte equivalence with x64.rs ─────────────────────────────

    #[test]
    fn mov_r64_r64_matches_emit_mov_reg_reg() {
        let mut r = Reference::new();
        for &dst in REGS.iter() {
            for &src in REGS.iter() {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_reg_reg(dst, src));
                let got = sel(&gpr(Op::Mov, Ty::I64, dst, src));
                assert_eq!(got, want, "MOV r{dst}, r{src}");
            }
        }
    }

    #[test]
    fn mov_r64_r64_store_form_matches_emit_mov_r64_r64() {
        let mut r = Reference::new();
        for &dst in REGS.iter() {
            for &src in REGS.iter() {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_r64_r64(dst, src));
                let a = Args {
                    dst,
                    src,
                    ..Args::default()
                };
                assert_eq!(
                    named("mov_r64_r64_store_form", &a),
                    want,
                    "MOV r/m64 r{dst}, r{src}"
                );
            }
        }
        // `emit_mov_rbp_rsp` is that same row with dst=RBP, src=RSP.
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_rbp_rsp());
        let a = Args {
            dst: RBP,
            src: RSP,
            ..Args::default()
        };
        assert_eq!(named("mov_r64_r64_store_form", &a), want, "MOV RBP, RSP");
        assert_eq!(want, vec![0x48, 0x89, 0xE5]);
    }

    #[test]
    fn mov_immediate_family_matches_the_shrinking_emitters() {
        let mut r = Reference::new();
        let values: [i64; 12] = [
            0,
            1,
            -1,
            127,
            -128,
            128,
            i64::from(i32::MAX),
            i64::from(i32::MIN),
            i64::from(i32::MAX) + 1,
            i64::from(i32::MIN) - 1,
            i64::MAX,
            i64::MIN,
        ];
        for &reg in REGS.iter() {
            for &v in values.iter() {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_imm64(reg, v));
                let got = sel(&gpr_imm(Op::Mov, Ty::I64, reg, v));
                assert_eq!(got, want, "MOV r{reg}, {v}");
            }
            // The i32-only entry point must agree over its own domain.
            for &v in values.iter() {
                if let Ok(v32) = i32::try_from(v) {
                    let want =
                        r.emit(|c: &mut super::super::Compiler| c.emit_mov_imm32_sx(reg, v32));
                    let got = sel(&gpr_imm(Op::Mov, Ty::I64, reg, v));
                    assert_eq!(got, want, "MOV r{reg}, {v32} (imm32 entry point)");
                }
            }
            // …and the non-shrinking form stays ten bytes for every value.
            for &v in values.iter() {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_imm64_full(reg, v));
                let a = Args {
                    dst: reg,
                    imm: v,
                    ..Args::default()
                };
                let got = named("mov_r64_imm64_full", &a);
                assert_eq!(got, want, "MOV r{reg}, {v} (full)");
                assert_eq!(got.len(), 10, "the IC sites depend on the fixed length");
            }
        }
    }

    #[test]
    fn xor_zeroing_matches_emit_xor_reg_self() {
        let mut r = Reference::new();
        for &reg in REGS.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_xor_reg_self(reg));
            let got = sel(&gpr_imm(Op::Mov, Ty::I64, reg, 0));
            assert_eq!(got, want, "XOR r{reg}, r{reg}");
        }
    }

    #[test]
    fn frame_slot_loads_and_stores_match_the_local_emitters() {
        let mut r = Reference::new();
        // Depths chosen around the disp8/disp32 boundary in both directions:
        // `modrm_rbp_disp` encodes `-depth`, so depth 128 is the last disp8.
        let depths: [i32; 10] = [0, 8, 16, 120, 127, 128, 129, 1024, -8, -128];
        for &reg in REGS.iter() {
            for &depth in depths.iter() {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_load_local(reg, depth));
                let mem = Mem::base_disp(RBP, -i64::from(depth));
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I64,
                    Operand::Gpr(reg),
                    Operand::Mem(mem),
                ));
                assert_eq!(got, want, "MOV r{reg}, [rbp - {depth}]");

                let want = r.emit(|c: &mut super::super::Compiler| c.emit_store_local(depth, reg));
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I64,
                    Operand::Mem(mem),
                    Operand::Gpr(reg),
                ));
                assert_eq!(got, want, "MOV [rbp - {depth}], r{reg}");

                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_lea_frame_slot(reg, depth));
                let got = sel(&Req::new(
                    Op::Lea,
                    Ty::I64,
                    Operand::Gpr(reg),
                    Operand::Mem(mem),
                ));
                assert_eq!(got, want, "LEA r{reg}, [rbp - {depth}]");
            }
            // Caller-arg loads are the same row with a POSITIVE displacement:
            // the shadow-space/stack-arg offsets that used to be narrowed with
            // `as u8`.
            for disp in [8i32, 16, 48, 127, 128, 256, 1024] {
                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_load_caller_arg(reg, disp));
                let mem = Mem::base_disp(RBP, i64::from(disp));
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I64,
                    Operand::Gpr(reg),
                    Operand::Mem(mem),
                ));
                assert_eq!(got, want, "MOV r{reg}, [rbp + {disp}]");
            }
        }
    }

    /// The RSP-relative store is the SIB special case: same row, different
    /// base.
    #[test]
    fn rsp_relative_store_matches_emit_mov_rsp_disp_from_reg() {
        let mut r = Reference::new();
        for &reg in REGS.iter() {
            for disp in [0i32, 8, 32, 127, 128, 4096, -8] {
                let want = r
                    .emit(|c: &mut super::super::Compiler| c.emit_mov_rsp_disp_from_reg(disp, reg));
                let mem = Mem::base_disp(RSP, i64::from(disp));
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I64,
                    Operand::Mem(mem),
                    Operand::Gpr(reg),
                ));
                assert_eq!(got, want, "MOV [rsp + {disp}], r{reg}");
                assert_eq!(got[2] & 7, 0b100, "r/m must say `SIB follows`");
                assert_eq!(got[3], 0x24, "index-free SIB byte");
            }
        }
    }

    #[test]
    fn disp32_memory_family_matches_the_tlab_and_field_emitters() {
        let mut r = Reference::new();
        let disps: [i32; 7] = [0, 8, 127, 128, 4096, -4096, i32::MAX];
        for &dst in REGS.iter() {
            for &base in non_sib_regs().iter() {
                for &disp in disps.iter() {
                    let mem = Mem::base_disp32(base, i64::from(disp));
                    let rm =
                        |op: Op, ty: Ty| Req::new(op, ty, Operand::Gpr(dst), Operand::Mem(mem));

                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_mov_r64_mem_disp32(dst, base, disp)
                    });
                    assert_eq!(sel(&rm(Op::Mov, Ty::I64)), want, "MOV r64 disp32");

                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_movsxd_r64_mem_disp32(dst, base, disp)
                    });
                    assert_eq!(sel(&rm(Op::Movsxd, Ty::I32)), want, "MOVSXD disp32");

                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_mov_r32_mem_disp32(dst, base, disp)
                    });
                    assert_eq!(sel(&rm(Op::Mov, Ty::I32)), want, "MOV r32 disp32");

                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_lea_r64_mem_disp32(dst, base, disp)
                    });
                    assert_eq!(sel(&rm(Op::Lea, Ty::I64)), want, "LEA disp32");

                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_cmp_r64_mem_disp32(dst, base, disp)
                    });
                    assert_eq!(sel(&rm(Op::Cmp, Ty::I64)), want, "CMP disp32");

                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_mov_mem_disp32_r64(base, dst, disp)
                    });
                    let got = sel(&Req::new(
                        Op::Mov,
                        Ty::I64,
                        Operand::Mem(mem),
                        Operand::Gpr(dst),
                    ));
                    assert_eq!(got, want, "MOV [disp32], r64");

                    for (bits, signed, op, ty) in [
                        (8u8, true, Op::Movsx, Ty::I8),
                        (8, false, Op::Movzx, Ty::I8),
                        (16, true, Op::Movsx, Ty::I16),
                        (16, false, Op::Movzx, Ty::I16),
                    ] {
                        let want = r.emit(|c: &mut super::super::Compiler| {
                            c.emit_movx_r64_mem_disp32(dst, base, disp, bits, signed)
                        });
                        let kind = if signed { "MOVSX" } else { "MOVZX" };
                        assert_eq!(sel(&rm(op, ty)), want, "{kind} {bits} disp32");
                    }
                }
            }
        }
    }

    #[test]
    fn dword_store_of_an_immediate_matches_emit_mov_dword_mem_disp32_imm32() {
        let mut r = Reference::new();
        for &base in non_sib_regs().iter() {
            for disp in [0i32, 64, 128, -4096] {
                for imm in [0i32, 1, -1, i32::MAX, i32::MIN] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_mov_dword_mem_disp32_imm32(base, disp, imm)
                    });
                    let mem = Mem::base_disp32(base, i64::from(disp));
                    let got = sel(&Req::new(
                        Op::Mov,
                        Ty::I32,
                        Operand::Mem(mem),
                        Operand::Imm(i64::from(imm)),
                    ));
                    assert_eq!(got, want, "MOV DWORD [r{base} + {disp}], {imm}");
                }
            }
        }
    }

    /// The stack-bang probe is the 32-bit load with an RSP base — the case
    /// `emit_mov_r32_mem_disp32` declines and the generic row handles.
    #[test]
    fn stack_bang_probe_matches_emit_stack_bang_load() {
        let mut r = Reference::new();
        for disp in [-4096i32, -8192, -128, -1] {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_stack_bang_load(disp));
            let mem = Mem::base_disp32(RSP, i64::from(disp));
            let got = sel(&Req::new(
                Op::Mov,
                Ty::I32,
                Operand::Gpr(RAX),
                Operand::Mem(mem),
            ));
            assert_eq!(got, want, "MOV EAX, [rsp + {disp}]");
            assert_eq!(&got[..3], &[0x8B, 0x84, 0x24][..]);
        }
    }

    #[test]
    fn byte_flag_test_matches_emit_test_mem8_imm8() {
        let mut r = Reference::new();
        for &base in non_sib_regs().iter() {
            for disp in [0i32, 8, 127, 128, 1024] {
                for imm in [0u8, 1, 0x80, 0xFF] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_test_mem8_imm8(base, disp, imm)
                    });
                    let mem = Mem::base_disp(base, i64::from(disp));
                    let got = sel(&Req::new(
                        Op::Test,
                        Ty::I8,
                        Operand::Mem(mem),
                        Operand::Imm(i64::from(imm)),
                    ));
                    assert_eq!(got, want, "TEST BYTE [r{base} + {disp}], {imm:#04X}");
                }
            }
        }
        // The zero-displacement safepoint poll keeps its explicit disp8 byte:
        // shrinking it to `mod=00` would move every following instruction.
        let mem = Mem::base_disp(RAX, 0);
        let got = sel(&Req::new(
            Op::Test,
            Ty::I8,
            Operand::Mem(mem),
            Operand::Imm(0x80),
        ));
        assert_eq!(got, vec![0xF6, 0x40, 0x00, 0x80]);
    }

    #[test]
    fn indexed_byte_store_matches_emit_mov_mem8_indexed_imm8() {
        let mut r = Reference::new();
        // The legacy emitter writes a literal `mod=00` ModRM byte, so it is
        // only correct for bases that HAVE a `mod=00` form. RBP/R13 are
        // excluded here and rejected by the row's own constraint check below.
        let bases: Vec<u8> = REGS.iter().copied().filter(|b| b & 7 != 0b101).collect();
        for &base in bases.iter() {
            for &index in REGS.iter() {
                if index == RSP {
                    continue;
                }
                for value in [0u8, 1, 0xFF] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_mov_mem8_indexed_imm8(base, index, value)
                    });
                    let mem = Mem::base_index(base, index, 1, 0);
                    let got = sel(&Req::new(
                        Op::Mov,
                        Ty::I8,
                        Operand::Mem(mem),
                        Operand::Imm(i64::from(value)),
                    ));
                    assert_eq!(got, want, "MOV BYTE [r{base} + r{index}], {value}");
                }
            }
        }
    }

    #[test]
    fn alu_and_compare_families_match_their_emitters() {
        let mut r = Reference::new();
        for &dst in REGS.iter() {
            for &src in REGS.iter() {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_sub_r64_r64(dst, src));
                assert_eq!(sel(&gpr(Op::Sub, Ty::I64, dst, src)), want, "SUB r64");

                let want = r.emit(|c: &mut super::super::Compiler| c.emit_cmp_r64_r64(dst, src));
                assert_eq!(sel(&gpr(Op::Cmp, Ty::I64, dst, src)), want, "CMP r64");

                let want = r.emit(|c: &mut super::super::Compiler| c.emit_cmp_r32_r32(dst, src));
                assert_eq!(sel(&gpr(Op::Cmp, Ty::I32, dst, src)), want, "CMP r32");

                for opcode in [0x01u8, 0x29, 0x21, 0x09, 0x31, 0x39, 0x85] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_alu_r32_r32(opcode, dst, src)
                    });
                    let got =
                        sel(&gpr(Op::Alu, Ty::I32, dst, src).with_extra(Operand::OpByte(opcode)));
                    assert_eq!(got, want, "ALU {opcode:#04X} r32");
                }

                for cc in [0x44u8, 0x45, 0x4C, 0x4D, 0x4E, 0x4F] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_cmov_cc_reg_reg(cc, dst, src)
                    });
                    let got = sel(&gpr(Op::Cmov, Ty::I64, dst, src).with_extra(Operand::Cc(cc)));
                    assert_eq!(got, want, "CMOV{cc:#04X}");
                }
            }
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r64_r64(dst));
            assert_eq!(sel(&gpr(Op::Test, Ty::I64, dst, dst)), want, "TEST r64");

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r32_r32(dst));
            assert_eq!(sel(&gpr(Op::Test, Ty::I32, dst, dst)), want, "TEST r32");

            for imm in [0i8, 1, -1, 7, -8, i8::MAX, i8::MIN] {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_add_r64_imm8(dst, imm));
                assert_eq!(
                    sel(&gpr_imm(Op::Add, Ty::I64, dst, i64::from(imm))),
                    want,
                    "ADD r{dst}, {imm}"
                );

                let want = r.emit(|c: &mut super::super::Compiler| c.emit_and_r64_imm8(dst, imm));
                assert_eq!(
                    sel(&gpr_imm(Op::And, Ty::I64, dst, i64::from(imm))),
                    want,
                    "AND r{dst}, {imm}"
                );

                let want = r.emit(|c: &mut super::super::Compiler| c.emit_or_r64_imm8(dst, imm));
                assert_eq!(
                    sel(&gpr_imm(Op::Or, Ty::I64, dst, i64::from(imm))),
                    want,
                    "OR r{dst}, {imm}"
                );
            }

            for imm in [0i32, 1, -1, 0x8000_0000u32 as i32, i32::MAX] {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r64_imm32(dst, imm));
                assert_eq!(
                    sel(&gpr_imm(Op::Test, Ty::I64, dst, i64::from(imm))),
                    want,
                    "TEST r{dst}, {imm}"
                );
            }

            for shift in [0u8, 1, 3, 32, 63] {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_shr_r64_imm8(dst, shift));
                assert_eq!(
                    sel(&gpr_imm(Op::Shr, Ty::I64, dst, i64::from(shift))),
                    want,
                    "SHR r{dst}, {shift}"
                );
            }
        }
    }

    /// The frame-adjust emitters, over the domain their call sites use.
    ///
    /// `emit_prologue`, `emit_epilogue` and the stack-arg block all pass a
    /// non-negative byte count; `stack_arg_block_size` cannot return a
    /// negative one. See `known_divergence_*` below for what happens outside
    /// that domain.
    #[test]
    fn frame_adjust_matches_emit_sub_add_rsp_imm_over_their_call_domain() {
        let mut r = Reference::new();
        for imm in [0i32, 8, 0x28, 64, 127, 128, 256, 4096, 1 << 20] {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_sub_rsp_imm(imm));
            assert_eq!(
                sel(&gpr_imm(Op::Sub, Ty::I64, RSP, i64::from(imm))),
                want,
                "SUB RSP, {imm}"
            );
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_add_rsp_imm(imm));
            assert_eq!(
                sel(&gpr_imm(Op::Add, Ty::I64, RSP, i64::from(imm))),
                want,
                "ADD RSP, {imm}"
            );
        }
    }

    /// A NEGATIVE frame adjustment is the one place the table and the
    /// hand-written emitter disagree, and the disagreement is size-only.
    ///
    /// `emit_sub_rsp_imm` tests `(0..=127).contains(&imm)`, so a negative
    /// immediate takes its `imm32` branch even though `imm8` would express
    /// it. The table picks the smallest legal form. No call site reaches
    /// this — every caller passes a byte count — but the difference is
    /// pinned here so a migration does not discover it in the field.
    #[test]
    fn known_divergence_negative_rsp_adjust_is_shorter_in_the_table() {
        let mut r = Reference::new();
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_sub_rsp_imm(-8));
        let got = sel(&gpr_imm(Op::Sub, Ty::I64, RSP, -8));
        assert_eq!(want, vec![0x48, 0x81, 0xEC, 0xF8, 0xFF, 0xFF, 0xFF]);
        assert_eq!(got, vec![0x48, 0x83, 0xEC, 0xF8]);
        // Same instruction, same operand value; only the immediate width
        // differs, and both are sign-extended to 64 bits.
        let d_want = decode(&want, true).expect("decodable");
        let d_got = decode(&got, true).expect("decodable");
        assert_eq!(d_want.reg(), d_got.reg(), "same /5 SUB extension");
        assert_eq!(d_want.rm_reg(), d_got.rm_reg(), "same destination");
        assert_eq!(
            i32::from_le_bytes([d_want.imm[0], d_want.imm[1], d_want.imm[2], d_want.imm[3]]),
            i32::from(d_got.imm[0] as i8),
            "same immediate value"
        );
    }

    #[test]
    fn stack_and_control_flow_match_their_emitters() {
        let mut r = Reference::new();
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_push_rbp());
        let a = Args {
            dst: RBP,
            ..Args::default()
        };
        assert_eq!(named("push_r64", &a), want, "PUSH RBP");
        assert_eq!(want, vec![0x55]);

        let want = r.emit(|c: &mut super::super::Compiler| c.emit_pop_rbp());
        assert_eq!(named("pop_r64", &a), want, "POP RBP");
        assert_eq!(want, vec![0x5D]);

        let want = r.emit(|c: &mut super::super::Compiler| c.emit_ret());
        assert_eq!(
            sel(&Req::new(Op::Ret, Ty::Void, Operand::None, Operand::None)),
            want,
            "RET"
        );

        // Extended registers pick up REX.B in the PlusReg forms.
        let a = Args {
            dst: R12,
            ..Args::default()
        };
        assert_eq!(named("push_r64", &a), vec![0x41, 0x54], "PUSH R12");
        assert_eq!(named("pop_r64", &a), vec![0x41, 0x5C], "POP R12");
    }

    /// The `Jcc`/`JMP` rows must reproduce both the bytes AND the patch site
    /// their emitters hand back to the caller.
    #[test]
    fn branch_patch_sites_match_the_emitters_returned_offsets() {
        let mut r = Reference::new();
        let mut want_patch = 0usize;
        let want = r.emit(|c: &mut super::super::Compiler| {
            let start = c.buf.pos();
            let p = c.emit_jmp_rel32_patch();
            want_patch = p - start;
        });
        let sel_jmp = select(&Req::new(
            Op::Jmp,
            Ty::Void,
            Operand::Rel32(0),
            Operand::None,
        ))
        .expect("jmp rel32");
        assert_eq!(sel_jmp.encoded.bytes, want, "JMP rel32");
        assert_eq!(sel_jmp.encoded.imm_offset, Some(want_patch));
        assert_eq!(want_patch, 1);

        for cc in [0x84u8, 0x85, 0x8C, 0x8F] {
            let mut want_patch = 0usize;
            let want = r.emit(|c: &mut super::super::Compiler| {
                let start = c.buf.pos();
                let p = c.emit_jcc_rel32_patch(cc);
                want_patch = p - start;
            });
            let s = select(
                &Req::new(Op::Jcc, Ty::Void, Operand::Rel32(0), Operand::None)
                    .with_extra(Operand::Cc(cc)),
            )
            .expect("jcc rel32");
            assert_eq!(s.encoded.bytes, want, "Jcc {cc:#04X}");
            assert_eq!(s.encoded.imm_offset, Some(want_patch));
            assert_eq!(want_patch, 2);
        }
    }

    #[test]
    fn xmm_family_matches_its_emitters() {
        let mut r = Reference::new();
        for &xmm in REGS.iter() {
            for &gp in REGS.iter() {
                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_movq_xmm_from_gpr(xmm, gp));
                let got = sel(&Req::new(
                    Op::Movq,
                    Ty::I64,
                    Operand::Xmm(xmm),
                    Operand::Gpr(gp),
                ));
                assert_eq!(got, want, "MOVQ xmm{xmm}, r{gp}");

                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_movq_gpr_from_xmm(gp, xmm));
                let got = sel(&Req::new(
                    Op::Movq,
                    Ty::I64,
                    Operand::Gpr(gp),
                    Operand::Xmm(xmm),
                ));
                assert_eq!(got, want, "MOVQ r{gp}, xmm{xmm}");
            }
            for &src in REGS.iter() {
                let want = r.emit(|c: &mut super::super::Compiler| c.emit_movsd_xmm_xmm(xmm, src));
                let got = sel(&Req::new(
                    Op::Movsd,
                    Ty::F64,
                    Operand::Xmm(xmm),
                    Operand::Xmm(src),
                ));
                assert_eq!(got, want, "MOVSD xmm{xmm}, xmm{src}");

                let want = r.emit(|c: &mut super::super::Compiler| c.emit_movss_xmm_xmm(xmm, src));
                let got = sel(&Req::new(
                    Op::Movss,
                    Ty::F32,
                    Operand::Xmm(xmm),
                    Operand::Xmm(src),
                ));
                assert_eq!(got, want, "MOVSS xmm{xmm}, xmm{src}");
            }
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_pxor_xmm_self(xmm));
            let got = sel(&Req::new(
                Op::Pxor,
                Ty::I64,
                Operand::Xmm(xmm),
                Operand::None,
            ));
            assert_eq!(got, want, "PXOR xmm{xmm}");

            for depth in [0i32, 8, 127, 128, 1024] {
                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_movq_mem_rbp_from_xmm(depth, xmm)
                });
                let mem = Mem::base_disp(RBP, -i64::from(depth));
                let got = sel(&Req::new(
                    Op::Movq,
                    Ty::I64,
                    Operand::Mem(mem),
                    Operand::Xmm(xmm),
                ));
                assert_eq!(got, want, "MOVQ [rbp - {depth}], xmm{xmm}");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_movq_xmm_from_mem_rbp(xmm, depth)
                });
                let got = sel(&Req::new(
                    Op::Movq,
                    Ty::I64,
                    Operand::Xmm(xmm),
                    Operand::Mem(mem),
                ));
                assert_eq!(got, want, "MOVQ xmm{xmm}, [rbp - {depth}]");
            }
        }
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_sqrtsd_xmm0());
        let got = sel(&Req::new(
            Op::Sqrtsd,
            Ty::F64,
            Operand::Xmm(0),
            Operand::Xmm(0),
        ));
        assert_eq!(got, want, "SQRTSD xmm0, xmm0");
        assert_eq!(want, vec![0xF2, 0x0F, 0x51, 0xC0]);
    }

    /// The rows whose ground truth is an inline byte literal rather than a
    /// named emitter. The literal and its `x64.rs` location are quoted in the
    /// row's `emitter` field.
    #[test]
    fn inline_byte_literal_rows_reproduce_those_literals() {
        // SHL/SAR/SHR EAX, CL — the `ishl`/`ishr`/`iushr` arithmetic steps.
        assert_eq!(
            sel(&Req::new(
                Op::Shl,
                Ty::I32,
                Operand::Gpr(RAX),
                Operand::None
            )),
            vec![0xD3, 0xE0]
        );
        assert_eq!(
            sel(&Req::new(
                Op::Sar,
                Ty::I32,
                Operand::Gpr(RAX),
                Operand::None
            )),
            vec![0xD3, 0xF8]
        );
        assert_eq!(
            sel(&Req::new(
                Op::Shr,
                Ty::I32,
                Operand::Gpr(RAX),
                Operand::None
            )),
            vec![0xD3, 0xE8]
        );
        // IMUL EAX, ECX and the MOVSXD RAX, EAX that follows it.
        assert_eq!(
            sel(&gpr(Op::Imul, Ty::I32, RAX, RCX)),
            vec![0x0F, 0xAF, 0xC1]
        );
        assert_eq!(
            sel(&gpr(Op::Movsxd, Ty::I32, RAX, RAX)),
            vec![0x48, 0x63, 0xC0]
        );
        // AND EAX, ECX through the parametric ALU row.
        assert_eq!(
            sel(&gpr(Op::Alu, Ty::I32, RAX, RCX).with_extra(Operand::OpByte(0x21))),
            vec![0x21, 0xC8]
        );
        // CQO ; IDIV RCX  and  CDQ ; IDIV ECX.
        assert_eq!(
            sel(&Req::new(
                Op::SignExtendAcc,
                Ty::I64,
                Operand::None,
                Operand::None
            )),
            vec![0x48, 0x99]
        );
        assert_eq!(
            sel(&Req::new(
                Op::Idiv,
                Ty::I64,
                Operand::Gpr(RCX),
                Operand::None
            )),
            vec![0x48, 0xF7, 0xF9]
        );
        assert_eq!(
            sel(&Req::new(
                Op::SignExtendAcc,
                Ty::I32,
                Operand::None,
                Operand::None
            )),
            vec![0x99]
        );
        assert_eq!(
            sel(&Req::new(
                Op::Idiv,
                Ty::I32,
                Operand::Gpr(RCX),
                Operand::None
            )),
            vec![0xF7, 0xF9]
        );
    }

    // ── displacement forms ────────────────────────────────────────────────

    /// Every memory row must resolve its displacement through `disp.rs` and
    /// must round-trip: what the decoder reads back is what the caller asked
    /// for, and the `mod` field agrees with the emitted width.
    #[test]
    fn displacement_forms_round_trip_through_disp() {
        let values: [i64; 13] = [
            0,
            1,
            -1,
            8,
            127,
            -128,
            128,
            -129,
            1024,
            -1024,
            65_536,
            i64::from(i32::MAX),
            i64::from(i32::MIN),
        ];
        for p in PATTERNS {
            if !p.is_memory() {
                continue;
            }
            for &base in REGS.iter() {
                if p.constraints.contains(&Constraint::NoSibBase) && base_requires_sib(base) {
                    continue;
                }
                for &v in values.iter() {
                    let mem = Mem {
                        base,
                        index: if p.constraints.contains(&Constraint::IndexNotRsp) {
                            Some(Index { reg: RCX, scale: 1 })
                        } else {
                            None
                        },
                        disp: v,
                        force_disp32: matches!(p.disp, DispPolicy::Force32),
                    };
                    let a = Args {
                        dst: RAX,
                        src: RCX,
                        mem,
                        imm: 0,
                        op_byte: 0,
                    };
                    let e = match p.encode(&a) {
                        Ok(e) => e,
                        Err(err) => panic!("`{}` base={base} disp={v}: {err}", p.name),
                    };
                    let d = decode(&e.bytes, p.enc.modrm)
                        .unwrap_or_else(|| panic!("`{}` produced undecodable bytes", p.name));
                    assert_eq!(
                        d.disp, v,
                        "`{}` base={base}: the CPU would read {} where {v} was asked for",
                        p.name, d.disp
                    );
                    assert_eq!(d.base(), Some(base), "`{}` lost the base register", p.name);
                    // `mod` and the emitted width must never disagree.
                    let expect_len = match d.mod_bits() {
                        Some(0b00) => 0,
                        Some(0b01) => 1,
                        Some(0b10) => 4,
                        other => panic!("`{}`: illegal mod field {other:?}", p.name),
                    };
                    assert_eq!(d.disp_len, expect_len, "`{}` mod/width disagree", p.name);
                    // The forced-width rows must stay four bytes wide even
                    // for a displacement of zero: their length is load-bearing.
                    if matches!(p.disp, DispPolicy::Force32) {
                        assert_eq!(d.disp_len, 4, "`{}` must stay disp32", p.name);
                    }
                    // RBP/R13 have no `mod=00` form.
                    if super::super::base_requires_displacement(base) {
                        assert_ne!(
                            d.mod_bits(),
                            Some(0b00),
                            "`{}` with base r{base} would decode as RIP-relative",
                            p.name
                        );
                    }
                    // RSP/R12 cannot be named without a SIB byte.
                    if base_requires_sib(base) {
                        assert!(d.sib.is_some(), "`{}` base r{base} needs a SIB", p.name);
                    }
                }
            }
        }
    }

    /// A displacement past disp32 has no encoding; every memory row must
    /// refuse it rather than truncate.
    #[test]
    fn unencodable_displacements_are_refused_by_every_memory_row() {
        for p in PATTERNS {
            if !p.is_memory() {
                continue;
            }
            let mem = Mem {
                base: RAX,
                index: if p.constraints.contains(&Constraint::IndexNotRsp) {
                    Some(Index { reg: RCX, scale: 1 })
                } else {
                    None
                },
                disp: 1i64 << 40,
                force_disp32: matches!(p.disp, DispPolicy::Force32),
            };
            let a = Args {
                dst: RAX,
                src: RCX,
                mem,
                imm: 0,
                op_byte: 0,
            };
            assert_eq!(
                p.encode(&a),
                Err(SelError::Disp(DispOutOfRange { value: 1i64 << 40 })),
                "`{}` must refuse a displacement with no encoding",
                p.name
            );
        }
    }

    // ── constraints ───────────────────────────────────────────────────────

    /// Constraint violations must come back as errors, not as bytes.
    #[test]
    fn constraint_violations_are_rejected_rather_than_encoded() {
        let base_args = Args {
            dst: RAX,
            src: RCX,
            mem: Mem::base_disp(RAX, 0),
            imm: 0,
            op_byte: 0,
        };

        // An RSP base in an encoding that emits no SIB byte would silently
        // become "a SIB byte follows" and misparse everything after it.
        let mut a = base_args;
        a.mem = Mem::base_disp(RSP, 0);
        a.imm = 0x80;
        assert_eq!(
            encode_named("test_m8_imm8", &a),
            Err(SelError::Constraint {
                pattern: "test_m8_imm8",
                constraint: Constraint::NoSibBase,
            })
        );

        // RSP can never be a SIB index.
        let mut a = base_args;
        a.mem = Mem::base_index(RAX, RSP, 1, 0);
        assert_eq!(
            encode_named("mov_m8_index_imm8", &a),
            Err(SelError::Constraint {
                pattern: "mov_m8_index_imm8",
                constraint: Constraint::IndexNotRsp,
            })
        );

        // A scale x86 cannot express.
        let mut a = base_args;
        a.mem = Mem::base_index(RAX, RCX, 3, 0);
        assert_eq!(
            encode_named("mov_m8_index_imm8", &a),
            Err(SelError::BadScale { scale: 3 })
        );

        // `TEST r,r` is a same-register idiom.
        let a = base_args;
        assert_eq!(
            encode_named("test_r64_r64", &a),
            Err(SelError::Constraint {
                pattern: "test_r64_r64",
                constraint: Constraint::SameRegister,
            })
        );

        // The condition byte families are disjoint: a Jcc byte is not a CMOV
        // byte, and swapping them would emit a different instruction.
        let mut a = base_args;
        a.op_byte = 0x84;
        assert_eq!(
            encode_named("cmov_r64_r64", &a),
            Err(SelError::Constraint {
                pattern: "cmov_r64_r64",
                constraint: Constraint::CcIsCmov,
            })
        );
        a.op_byte = 0x44;
        assert_eq!(
            encode_named("jcc_rel32", &a),
            Err(SelError::Constraint {
                pattern: "jcc_rel32",
                constraint: Constraint::CcIsJcc,
            })
        );

        // Register numbers outside the file.
        let mut a = base_args;
        a.dst = 16;
        assert_eq!(
            encode_named("mov_r64_r64", &a),
            Err(SelError::BadRegister {
                pattern: "mov_r64_r64",
                reg: 16,
            })
        );

        // An immediate that does not fit the declared field.
        let mut a = base_args;
        a.imm = 200;
        assert_eq!(
            encode_named("add_r64_imm8", &a),
            Err(SelError::Constraint {
                pattern: "add_r64_imm8",
                constraint: Constraint::ImmFitsI8,
            })
        );

        // A shift count past 63 is not a legal 64-bit shift.
        let mut a = base_args;
        a.imm = 64;
        assert_eq!(
            encode_named("shr_r64_imm8", &a),
            Err(SelError::Constraint {
                pattern: "shr_r64_imm8",
                constraint: Constraint::ShiftCountFits64,
            })
        );

        // And a request no row covers is an error, not a guess.
        assert!(matches!(
            select(&Req::new(
                Op::Sqrtsd,
                Ty::I8,
                Operand::Xmm(0),
                Operand::Xmm(0)
            )),
            Err(SelError::NoPattern { .. })
        ));
        assert_eq!(
            encode_named("no_such_pattern", &base_args),
            Err(SelError::UnknownPattern)
        );
    }

    // ── immediate-size selection ──────────────────────────────────────────

    /// The selector must pick the smallest legal immediate form, and the
    /// boundaries must be the *signed* ones.
    #[test]
    fn immediate_size_selection_picks_the_smallest_legal_form() {
        // MOV r64, imm: zero -> XOR, i32 -> C7, wider -> B8+rd.
        assert_eq!(
            select(&gpr_imm(Op::Mov, Ty::I64, RAX, 0))
                .expect("zero")
                .pattern
                .name,
            "mov_r64_imm0_xor"
        );
        for v in [
            1i64,
            -1,
            127,
            -128,
            128,
            i64::from(i32::MAX),
            i64::from(i32::MIN),
        ] {
            assert_eq!(
                select(&gpr_imm(Op::Mov, Ty::I64, RAX, v))
                    .expect("i32 range")
                    .pattern
                    .name,
                "mov_r64_imm32",
                "value {v}"
            );
        }
        for v in [i64::from(i32::MAX) + 1, i64::from(i32::MIN) - 1, i64::MAX] {
            assert_eq!(
                select(&gpr_imm(Op::Mov, Ty::I64, RAX, v))
                    .expect("wide")
                    .pattern
                    .name,
                "mov_r64_imm64",
                "value {v}"
            );
        }

        // ADD/SUB r64, imm: imm8 up to the SIGNED boundary, then imm32.
        for (op, small, big) in [
            (Op::Add, "add_r64_imm8", "add_r64_imm32"),
            (Op::Sub, "sub_r64_imm8", "sub_r64_imm32"),
        ] {
            for v in [0i64, 1, -1, 127, -128] {
                assert_eq!(
                    select(&gpr_imm(op, Ty::I64, RSP, v))
                        .expect("imm8")
                        .pattern
                        .name,
                    small,
                    "{op:?} {v} must use the byte form"
                );
            }
            for v in [128i64, -129, 4096, i64::from(i32::MAX)] {
                assert_eq!(
                    select(&gpr_imm(op, Ty::I64, RSP, v))
                        .expect("imm32")
                        .pattern
                        .name,
                    big,
                    "{op:?} {v} does not fit a SIGNED byte"
                );
            }
            assert!(
                select(&gpr_imm(op, Ty::I64, RSP, i64::from(i32::MAX) + 1)).is_err(),
                "{op:?}: no row can express an immediate past i32"
            );
        }

        // 128 is the value the whole `disp.rs` exercise exists for: it is a
        // legal u8 and NOT a legal signed imm8.
        let short = select(&gpr_imm(Op::Add, Ty::I64, RAX, 127)).expect("127");
        let long = select(&gpr_imm(Op::Add, Ty::I64, RAX, 128)).expect("128");
        assert_eq!(short.encoded.bytes.len() + 3, long.encoded.bytes.len());

        // The zeroing idiom is NOT flag-neutral. `emit_mov_imm64` substitutes
        // it unconditionally and so does the selector, which is faithful — but
        // the table records the difference, so a future caller that needs the
        // flags preserved across the move has something to consult instead of
        // reading the emitter.
        assert!(pattern("mov_r64_imm0_xor").expect("row").flags.writes);
        assert!(!pattern("mov_r64_imm32").expect("row").flags.writes);
        assert!(!pattern("mov_r64_imm64").expect("row").flags.writes);
    }

    /// The smallest-form and forced-width memory rows are different answers
    /// to different questions; the matcher must not substitute one for the
    /// other.
    #[test]
    fn forced_width_and_smallest_form_rows_do_not_shadow_each_other() {
        let smallest = select(&Req::new(
            Op::Mov,
            Ty::I64,
            Operand::Gpr(RAX),
            Operand::Mem(Mem::base_disp(RCX, 8)),
        ))
        .expect("smallest");
        assert_eq!(smallest.pattern.name, "mov_r64_m");
        assert_eq!(smallest.encoded.bytes.len(), 4);

        let forced = select(&Req::new(
            Op::Mov,
            Ty::I64,
            Operand::Gpr(RAX),
            Operand::Mem(Mem::base_disp32(RCX, 8)),
        ))
        .expect("forced");
        assert_eq!(forced.pattern.name, "mov_r64_m_disp32");
        assert_eq!(forced.encoded.bytes.len(), 7);
    }

    // ── round-trip disassembly ────────────────────────────────────────────

    /// Encode, then take the bytes apart again with a decoder that knows only
    /// x86's structural rules, and check that every operand comes back.
    #[test]
    fn round_trip_disassembly_recovers_every_operand() {
        for p in PATTERNS {
            for &r1 in REGS.iter() {
                for &r2 in REGS.iter() {
                    if p.constraints.contains(&Constraint::NoSibBase) && base_requires_sib(r2) {
                        continue;
                    }
                    if p.constraints.contains(&Constraint::IndexNotRsp) && r2 == RSP {
                        continue;
                    }
                    let a = args_for(p, r1, r2, r2, RCX, 64);
                    let mem = a.mem;
                    let e = match p.encode(&a) {
                        Ok(e) => e,
                        Err(err) => panic!("`{}` r{r1}/r{r2}: {err}", p.name),
                    };
                    if e.bytes.is_empty() {
                        // The elided self-move: nothing to decode.
                        assert_eq!(p.peephole, Peephole::ElideWhenDstEqSrc);
                        assert_eq!(a.dst, a.src);
                        continue;
                    }
                    let d = decode(&e.bytes, p.enc.modrm)
                        .unwrap_or_else(|| panic!("`{}` produced undecodable bytes", p.name));
                    let text = render(p, &d);
                    assert_eq!(d.len, e.bytes.len(), "{text}: decoder length");
                    assert!(
                        !text.is_empty(),
                        "every pattern must disassemble to something readable"
                    );

                    // The opcode came back intact.
                    match p.enc.opcode {
                        Opcode::One(b) => assert_eq!(d.opcode, vec![b], "`{}`", p.name),
                        Opcode::Two(b) => assert_eq!(d.opcode, vec![0x0F, b], "`{}`", p.name),
                        Opcode::PlusReg(b) => {
                            assert_eq!(d.opcode, vec![b | (a.dst & 7)], "`{}`", p.name)
                        }
                        Opcode::OperandOne => {
                            assert_eq!(d.opcode, vec![a.op_byte], "`{}`", p.name)
                        }
                        Opcode::OperandTwo => {
                            assert_eq!(d.opcode, vec![0x0F, a.op_byte], "`{}`", p.name)
                        }
                    }
                    assert_eq!(d.wide(), p.enc.rex_w, "`{}` REX.W", p.name);

                    // The register fields came back intact.
                    if p.enc.modrm {
                        let want_reg = match p.enc.reg {
                            RegF::Dst => a.dst,
                            RegF::Src => a.src,
                            RegF::Ext(n) => n,
                        };
                        assert_eq!(d.reg(), Some(want_reg), "`{}` reg field", p.name);
                        match p.enc.rm {
                            RmF::RegDst => {
                                assert_eq!(d.mod_bits(), Some(0b11), "`{}`", p.name);
                                assert_eq!(d.rm_reg(), Some(a.dst), "`{}` r/m", p.name);
                            }
                            RmF::RegSrc => {
                                assert_eq!(d.mod_bits(), Some(0b11), "`{}`", p.name);
                                assert_eq!(d.rm_reg(), Some(a.src), "`{}` r/m", p.name);
                            }
                            RmF::Mem => {
                                assert_eq!(d.base(), Some(mem.base), "`{}` base", p.name);
                                assert_eq!(d.disp, mem.disp, "`{}` disp", p.name);
                                match mem.index {
                                    Some(ix) => {
                                        assert_eq!(d.index(), Some(ix.reg), "`{}`", p.name);
                                        assert_eq!(d.scale(), Some(ix.scale), "`{}`", p.name);
                                    }
                                    None => assert_eq!(d.index(), None, "`{}`", p.name),
                                }
                            }
                            RmF::None => {}
                        }
                    }

                    // The immediate came back intact and at the offset the
                    // pattern advertised.
                    assert_eq!(
                        d.imm.len(),
                        p.imm.byte_len(),
                        "`{}` immediate width",
                        p.name
                    );
                    if p.imm.byte_len() > 0 {
                        assert_eq!(
                            e.imm_offset,
                            Some(e.bytes.len() - p.imm.byte_len()),
                            "`{}` immediate offset",
                            p.name
                        );
                    }
                }
            }
        }
    }

    /// Spot-check the decoder itself against hand-verified encodings, so a
    /// bug in the decoder cannot quietly validate a bug in the encoder.
    #[test]
    fn the_decoder_agrees_with_hand_verified_bytes() {
        // 4C 8B 7C 24 08 = MOV R15, [RSP + 8]
        let d = decode(&[0x4C, 0x8B, 0x7C, 0x24, 0x08], true).expect("decodable");
        assert_eq!(d.rex, Some(0x4C));
        assert!(d.wide());
        assert_eq!(d.opcode, vec![0x8B]);
        assert_eq!(d.reg(), Some(R15));
        assert_eq!(d.base(), Some(RSP));
        assert_eq!(d.index(), None);
        assert_eq!(d.disp, 8);
        assert_eq!(d.mod_bits(), Some(0b01));

        // 49 8B 45 00 = MOV RAX, [R13 + 0] — the base with no mod=00 form.
        let d = decode(&[0x49, 0x8B, 0x45, 0x00], true).expect("decodable");
        assert_eq!(d.reg(), Some(RAX));
        assert_eq!(d.base(), Some(R13));
        assert_eq!(d.disp, 0);
        assert_eq!(d.disp_len, 1);

        // 42 C6 04 08 FF = MOV BYTE [RAX + R9], 0xFF (REX.X only).
        let d = decode(&[0x42, 0xC6, 0x04, 0x08, 0xFF], true).expect("decodable");
        assert_eq!(d.base(), Some(RAX));
        assert_eq!(d.index(), Some(R9));
        assert_eq!(d.scale(), Some(1));
        assert_eq!(d.imm, vec![0xFF]);

        // 0F 8F 00 00 00 00 = JG rel32, no ModRM.
        let d = decode(&[0x0F, 0x8F, 0, 0, 0, 0], false).expect("decodable");
        assert_eq!(d.opcode, vec![0x0F, 0x8F]);
        assert_eq!(d.modrm, None);
        assert_eq!(d.imm, vec![0, 0, 0, 0]);

        // Truncated input is refused rather than mis-parsed.
        assert_eq!(decode(&[0x48], true), None);
        assert_eq!(decode(&[0x48, 0x8B], true), None);
        assert_eq!(decode(&[0x48, 0x8B, 0x80, 0x00], true), None);
        assert_eq!(decode(&[], false), None);

        // A disp8 byte is read back SIGNED — the property `disp.rs` exists to
        // preserve, restated at the decoder so the round trip is meaningful.
        let d = decode(&[0x48, 0x8B, 0x41, 0x80], true).expect("decodable");
        assert_eq!(d.disp, -128, "0x80 is -128, not 128");
    }

    /// A pattern whose bytes cannot be decoded structurally would make every
    /// round-trip assertion vacuous.
    #[test]
    fn every_pattern_produces_structurally_decodable_bytes() {
        let mut decoded_rows = 0usize;
        for p in PATTERNS {
            let a = args_for(p, R8, R11, RBX, RDI, -8);
            let e = match p.encode(&a) {
                Ok(e) => e,
                Err(err) => panic!("`{}`: {err}", p.name),
            };
            assert!(
                decode(&e.bytes, p.enc.modrm).is_some(),
                "`{}` emitted bytes no decoder can parse: {:02X?}",
                p.name,
                e.bytes
            );
            decoded_rows += 1;
        }
        assert_eq!(decoded_rows, PATTERNS.len());
    }

    // ══════════════════════════════════════════════════════════════════════
    // IR-level instruction selection
    // ══════════════════════════════════════════════════════════════════════

    use crate::ir::{MemKind, SafepointSnapshot, NO_NODE};

    /// A bare graph. `Graph`'s fields are public and `UseLists` derives
    /// `Default`, which is how `ir_schedule`'s own tests build one.
    fn g() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: NO_NODE,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
        }
    }

    fn konst(graph: &mut Graph, v: i64) -> NodeId {
        graph.add(IrOp::Const(v), IrType::Long, vec![], None)
    }

    fn konst_i32(graph: &mut Graph, v: i64) -> NodeId {
        graph.add(IrOp::Const(v), IrType::Int, vec![], None)
    }

    fn param(graph: &mut Graph, i: u16) -> NodeId {
        graph.add(IrOp::Param(i), IrType::Long, vec![], None)
    }

    fn bin(graph: &mut Graph, op: IrOp, a: NodeId, b: NodeId) -> NodeId {
        graph.add(op, IrType::Long, vec![a, b], None)
    }

    /// Give `id` a second consumer so it is no longer single-use. Returns the
    /// consumer, which the caller can leave out of the block under test.
    fn second_use(graph: &mut Graph, id: NodeId) -> NodeId {
        graph.add(IrOp::Neg, IrType::Long, vec![id], None)
    }

    fn ctx_of<'a>(graph: &'a Graph, block: &[NodeId]) -> SelCtx<'a> {
        SelCtx::new(graph, block)
    }

    // ── the new ALU rows are the byte literals they claim ─────────────────

    /// Every row added for the IR selector names an `ir_lower.rs` byte
    /// literal in its `emitter` field. This is that claim, checked: if a row
    /// and the literal it cites ever disagree, the table is lying about what
    /// the compiler emits and the selector's cost model is priced off a
    /// fiction.
    #[test]
    fn the_new_alu_rows_reproduce_the_ir_lower_byte_literals() {
        // `ir_lower::lower_data_node`, Op::Add / Sub / Mul / And / Or / Xor.
        assert_eq!(
            sel(&gpr(Op::Add, Ty::I64, RAX, RCX)),
            vec![0x48, 0x01, 0xC8]
        );
        assert_eq!(sel(&gpr(Op::Add, Ty::I32, RAX, RCX)), vec![0x01, 0xC8]);
        assert_eq!(
            sel(&gpr(Op::Sub, Ty::I64, RAX, RCX)),
            vec![0x48, 0x29, 0xC8]
        );
        assert_eq!(sel(&gpr(Op::Sub, Ty::I32, RAX, RCX)), vec![0x29, 0xC8]);
        assert_eq!(
            sel(&gpr(Op::And, Ty::I64, RAX, RCX)),
            vec![0x48, 0x21, 0xC8]
        );
        assert_eq!(sel(&gpr(Op::Or, Ty::I64, RAX, RCX)), vec![0x48, 0x09, 0xC8]);
        assert_eq!(
            sel(&gpr(Op::Xor, Ty::I64, RAX, RCX)),
            vec![0x48, 0x31, 0xC8]
        );
        assert_eq!(
            sel(&gpr(Op::Imul, Ty::I64, RAX, RCX)),
            vec![0x48, 0x0F, 0xAF, 0xC1]
        );
        assert_eq!(
            sel(&gpr(Op::Imul, Ty::I32, RAX, RCX)),
            vec![0x0F, 0xAF, 0xC1]
        );
    }

    /// The zeroing idiom and a real `XOR` share an opcode; they must not share
    /// a row, or a request for `xor a, b` could be answered with `a ^ a`.
    #[test]
    fn the_xor_zeroing_idiom_and_a_real_xor_are_different_rows() {
        let zeroing = select(&gpr_imm(Op::Mov, Ty::I64, RDX, 0)).expect("zeroing");
        assert_eq!(zeroing.pattern.name, "mov_r64_imm0_xor");
        let real = select(&gpr(Op::Xor, Ty::I64, RDX, RSI)).expect("xor");
        assert_eq!(real.pattern.name, "xor_r64_r64");
        assert_ne!(zeroing.encoded.bytes, real.encoded.bytes);
    }

    // ── immediate widths ──────────────────────────────────────────────────

    /// The whole point of `imm_form_for`: 128 is NOT an `imm8`. A raw
    /// `128 as i8` is `-128`, so `AND r, 128` written that way clears every
    /// bit but the sign bit instead of setting one.
    #[test]
    fn immediate_widths_use_checked_conversions_not_casts() {
        assert_eq!(imm_form_for(0), Some(ImmForm::Imm8));
        assert_eq!(imm_form_for(127), Some(ImmForm::Imm8));
        assert_eq!(imm_form_for(-128), Some(ImmForm::Imm8));
        assert_eq!(imm_form_for(128), Some(ImmForm::Imm32));
        assert_eq!(imm_form_for(-129), Some(ImmForm::Imm32));
        assert_eq!(imm_form_for(i64::from(i32::MAX)), Some(ImmForm::Imm32));
        assert_eq!(imm_form_for(i64::from(i32::MIN)), Some(ImmForm::Imm32));
        assert_eq!(imm_form_for(i64::from(i32::MAX) + 1), None);
        assert_eq!(imm_form_for(i64::from(i32::MIN) - 1), None);
        assert_eq!(imm_form_for(i64::MAX), None);
        // Restate the bug the checked form prevents.
        assert_eq!(128u8 as i8, -128);
    }

    /// A constant too wide for any immediate field must fall back to the
    /// register form, and say so — never be truncated into the field.
    #[test]
    fn a_constant_past_imm32_falls_back_to_the_register_form() {
        let mut graph = g();
        let p = param(&mut graph, 0);
        let big = konst(&mut graph, i64::from(i32::MAX) + 1);
        let add = bin(&mut graph, IrOp::Add, p, big);
        let block = vec![big, add];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        assert!(s.covers(&block), "coverage: {s:?}");
        let root = s
            .tiles
            .iter()
            .find(|t| t.root == add)
            .expect("the add is a tile root");
        assert_eq!(
            root.rule,
            Rule::AluReg,
            "a wide constant must not become an immediate"
        );
        assert!(
            s.notes.iter().any(|n| matches!(
                n,
                Note::WideImmediate { value, .. } if *value == i64::from(i32::MAX) + 1
            )),
            "the refusal must be reported: {:?}",
            s.notes
        );
    }

    #[test]
    fn a_constant_that_fits_imm8_becomes_an_immediate() {
        let mut graph = g();
        let p = param(&mut graph, 0);
        let k = konst(&mut graph, 24);
        let add = bin(&mut graph, IrOp::Add, p, k);
        let block = vec![k, add];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        assert!(s.covers(&block));
        let root = s.tiles.iter().find(|t| t.root == add).expect("root");
        assert_eq!(root.rule, Rule::AluImm);
        assert!(
            root.covered.contains(&k),
            "the constant must be absorbed, not left to emit a dead MOV"
        );
        assert!(root.insts.iter().any(|i| matches!(
            i,
            MInst::AluRI {
                op: Op::Add,
                form: ImmForm::Imm8,
                ..
            }
        )));
    }

    // ── address-mode folding ──────────────────────────────────────────────

    #[test]
    fn address_folds_base_index_scale_and_displacement() {
        // p0 + (i << 2) + 16
        let mut graph = g();
        let p0 = param(&mut graph, 0);
        let i = param(&mut graph, 1);
        let two = konst(&mut graph, 2);
        let shl = bin(&mut graph, IrOp::Shl, i, two);
        let sixteen = konst(&mut graph, 16);
        let a1 = bin(&mut graph, IrOp::Add, p0, shl);
        let a2 = bin(&mut graph, IrOp::Add, a1, sixteen);
        let block = vec![two, shl, sixteen, a1, a2];
        let ctx = ctx_of(&graph, &block);
        let m = match_address(&ctx, a2).expect("an address");
        assert_eq!(m.addr.base, Some(p0));
        assert_eq!(m.addr.index, Some(i));
        assert_eq!(m.addr.scale, 4);
        assert_eq!(m.addr.disp, 16);
        assert_eq!(m.addr.terms(), 3);
        for n in [a2, a1, shl, two, sixteen] {
            assert!(m.absorbed.contains(&n), "node {n} must be absorbed");
        }
        assert!(m.addr.check().is_ok());
    }

    /// An interior node with a second consumer keeps its register and becomes
    /// a plain term. Absorbing it would compute it twice.
    #[test]
    fn a_shared_interior_node_is_not_absorbed() {
        let mut graph = g();
        let p0 = param(&mut graph, 0);
        let i = param(&mut graph, 1);
        let two = konst(&mut graph, 2);
        let shl = bin(&mut graph, IrOp::Shl, i, two);
        let add = bin(&mut graph, IrOp::Add, p0, shl);
        // A second consumer of the shift, outside the block under test.
        let _other = second_use(&mut graph, shl);
        let block = vec![two, shl, add];
        let ctx = ctx_of(&graph, &block);
        let m = match_address(&ctx, add).expect("an address");
        assert_eq!(m.addr.base, Some(p0));
        assert_eq!(
            m.addr.index,
            Some(shl),
            "the shared shift stays a value and becomes the index itself"
        );
        assert_eq!(m.addr.scale, 1, "its scale is gone with it");
        assert!(!m.absorbed.contains(&shl));
    }

    /// A node the scheduler put in another block must not be pulled in: it
    /// would be recomputed here *and* still computed there.
    #[test]
    fn an_out_of_block_interior_node_is_not_absorbed() {
        let mut graph = g();
        let p0 = param(&mut graph, 0);
        let i = param(&mut graph, 1);
        let two = konst(&mut graph, 2);
        let shl = bin(&mut graph, IrOp::Shl, i, two);
        let add = bin(&mut graph, IrOp::Add, p0, shl);
        // `shl` is single-use but lives elsewhere.
        let block = vec![add];
        let ctx = ctx_of(&graph, &block);
        let m = match_address(&ctx, add).expect("an address");
        assert_eq!(m.addr.index, Some(shl));
        assert_eq!(m.addr.scale, 1);
        assert!(!m.absorbed.contains(&shl));
    }

    /// The checked-displacement gate. `Disp::encode32` refuses, so the address
    /// is refused — it is never truncated into a 32-bit field.
    #[test]
    fn a_displacement_past_disp32_is_refused_not_truncated() {
        let mut graph = g();
        let p0 = param(&mut graph, 0);
        let far = konst(&mut graph, 1i64 << 40);
        let add = bin(&mut graph, IrOp::Add, p0, far);
        let block = vec![far, add];
        let ctx = ctx_of(&graph, &block);
        assert_eq!(
            match_address(&ctx, add),
            Err(AddrRefusal::Unencodable(SelError::Disp(DispOutOfRange {
                value: 1i64 << 40
            })))
        );
        // And the selector must not choose an LEA it cannot encode.
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        assert!(s.covers(&block));
        assert!(
            !s.matched().any(|t| t.rule == Rule::Lea),
            "no LEA may survive an unencodable displacement"
        );
    }

    #[test]
    fn a_folded_displacement_cannot_wrap() {
        let mut graph = g();
        let p0 = param(&mut graph, 0);
        let a = konst(&mut graph, i64::MAX);
        let b = konst(&mut graph, 1);
        let s1 = bin(&mut graph, IrOp::Add, p0, a);
        let s2 = bin(&mut graph, IrOp::Add, s1, b);
        let block = vec![a, b, s1, s2];
        let ctx = ctx_of(&graph, &block);
        // `checked_add` refuses; a wrapping add would have produced
        // `i64::MIN`, which is not "a small displacement" either but *is* a
        // different, silently wrong answer.
        assert!(matches!(
            match_address(&ctx, s2),
            Err(AddrRefusal::DispOverflow(_)) | Err(AddrRefusal::Unencodable(_))
        ));
    }

    #[test]
    fn small_multiplies_become_two_term_addresses() {
        for (mult, want_scale) in [(3i64, 2u8), (5, 4), (9, 8)] {
            let mut graph = g();
            let x = param(&mut graph, 0);
            let k = konst(&mut graph, mult);
            let mul = bin(&mut graph, IrOp::Mul, x, k);
            let block = vec![k, mul];
            let ctx = ctx_of(&graph, &block);
            let m = match_address(&ctx, mul)
                .unwrap_or_else(|e| panic!("x*{mult} must be an address: {e:?}"));
            assert_eq!(m.addr.base, Some(x));
            assert_eq!(m.addr.index, Some(x));
            assert_eq!(m.addr.scale, want_scale);
            assert_eq!(m.addr.disp, 0);
            assert!(m.addr.check().is_ok());
        }
    }

    /// `x*4` needs `[x*4]` — no base — which `Mem` cannot express. Refuse
    /// rather than emit an operand nobody can lower.
    #[test]
    fn a_base_less_address_is_refused() {
        let mut graph = g();
        let x = param(&mut graph, 0);
        let k = konst(&mut graph, 4);
        let mul = bin(&mut graph, IrOp::Mul, x, k);
        let block = vec![k, mul];
        let ctx = ctx_of(&graph, &block);
        assert_eq!(
            match_address(&ctx, mul),
            Err(AddrRefusal::NotAnAddress(mul))
        );
        // And the type itself refuses a base-less operand outright.
        let bare = IrAddr {
            base: None,
            index: Some(x),
            scale: 4,
            disp: 0,
        };
        assert_eq!(bare.check(), Err(SelError::NoBaseRegister));
    }

    #[test]
    fn an_illegal_scale_is_refused() {
        let bad = IrAddr {
            base: Some(0),
            index: Some(1),
            scale: 3,
            disp: 0,
        };
        assert_eq!(bad.check(), Err(SelError::BadScale { scale: 3 }));
    }

    #[test]
    fn the_address_walk_is_bounded() {
        let mut graph = g();
        let mut acc = param(&mut graph, 0);
        let mut block = Vec::new();
        for n in 0..(ADDR_MATCH_BUDGET as i64 + 8) {
            let k = konst(&mut graph, n);
            acc = bin(&mut graph, IrOp::Add, acc, k);
            block.push(k);
            block.push(acc);
        }
        let ctx = ctx_of(&graph, &block);
        assert_eq!(match_address(&ctx, acc), Err(AddrRefusal::Budget));
    }

    // ── LEA versus the two-address ALU form ───────────────────────────────

    /// `LEA` is non-destructive, so it wins exactly when the left operand is
    /// still live and the ALU form would need a `MOV` first. This is the cost
    /// model doing real work: neither instruction is unconditionally better.
    #[test]
    fn lea_wins_only_when_the_alu_form_would_need_a_copy() {
        // (a) left operand dies here: plain ADD is cheaper.
        let mut graph = g();
        let p = param(&mut graph, 0);
        let q = param(&mut graph, 1);
        let add = bin(&mut graph, IrOp::Add, p, q);
        let block = vec![add];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        let t = s.tiles.iter().find(|t| t.root == add).expect("root");
        assert_eq!(t.rule, Rule::AluReg, "a dying operand needs no copy");

        // (b) left operand is live afterwards: the copy makes LEA cheaper.
        let mut graph = g();
        let p = param(&mut graph, 0);
        let q = param(&mut graph, 1);
        let add = bin(&mut graph, IrOp::Add, p, q);
        let _keep = second_use(&mut graph, p);
        let block = vec![add];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        let t = s.tiles.iter().find(|t| t.root == add).expect("root");
        assert_eq!(t.rule, Rule::Lea, "a live operand makes the copy real");
        assert!(matches!(t.insts.as_slice(), [MInst::Lea { .. }]));
    }

    /// The immediate form wins under frame homing, and loses without it.
    ///
    /// Measured, not assumed: with `frame_homed` off the level-2 encoder took
    /// 39 tiles across CratonBenchC2's three phases and `Rule::AluImm` was
    /// selected **zero** times — not because the rows were missing (increment 1
    /// added them) and not because the constant was folded away, but because
    /// `ADD EAX, ECX` is two bytes and `ADD EAX, 7` is three. The cost model
    /// prices instructions; under frame homing the register operand also costs
    /// a `MOV r64, [RBP-disp8]` that the immediate form does not, and pricing
    /// only the instruction hides four bytes and a micro-op.
    ///
    /// The constant is deliberately given a second consumer here, because that
    /// is what real code looks like: `ValueUses::single_use` requires
    /// `count == 1` AND not pinned, and a safepoint snapshot names almost every
    /// live constant — so the tile usually cannot absorb it and the two forms
    /// really are competing over one node.
    ///
    /// The exact edit that trips it: drop the `opts.frame_homed` arm from
    /// `tiles_alu`'s `extra`. The first assertion flips back to `AluReg`.
    #[test]
    fn frame_homing_makes_the_immediate_form_win() {
        let build = || {
            let mut graph = g();
            let p = param(&mut graph, 0);
            let k = konst(&mut graph, 7);
            let add = bin(&mut graph, IrOp::Add, p, k);
            // A second consumer for the CONSTANT only, so it is not absorbable
            // and both candidates cover exactly the add — and so the left
            // operand stays single-use, which keeps the two-address copy out of
            // the comparison. This is a test about operand loads, not copies.
            let _other = second_use(&mut graph, k);
            (graph, add)
        };

        let (graph, add) = build();
        let block = vec![add];
        let homed = SelectOptions {
            frame_homed: true,
            ..SelectOptions::default()
        };
        let s = select_block(&graph, &block, None, &homed);
        let t = s.tiles.iter().find(|t| t.root == add).expect("root");
        assert_eq!(
            t.rule,
            Rule::AluImm,
            "the immediate form drops a frame load the register form pays"
        );
        assert_eq!(t.covered.as_slice(), [add], "the constant is still live");

        // And the default is untouched, which is what keeps increments 0 and
        // 1's coverage figures comparable.
        let (graph, add) = build();
        let s = select_block(&graph, &vec![add], None, &SelectOptions::default());
        let t = s.tiles.iter().find(|t| t.root == add).expect("root");
        assert_eq!(t.rule, Rule::AluReg);
    }

    /// …and under [`SelectOptions::frame_homed`] the copy is *not* real, so the
    /// same graph selects the `ADD` again.
    ///
    /// This is the option earning its keep rather than being a preference. The
    /// consumer that sets it (`ir_lower`'s level-2 encoder) loads the left
    /// operand into the destination register whatever the tile says, so a
    /// `MInst::Move` prefix costs nothing and buys nothing — and priced as
    /// though it cost something it hands `a + b` an `LEA` a byte longer than
    /// the `ADD` it replaced.
    ///
    /// The exact edit that trips it: drop the `!opts.frame_homed &&` from
    /// `needs_copy`. Both assertions below flip.
    #[test]
    fn frame_homing_removes_the_copy_that_makes_lea_win() {
        let mut graph = g();
        let p = param(&mut graph, 0);
        let q = param(&mut graph, 1);
        let add = bin(&mut graph, IrOp::Add, p, q);
        let _keep = second_use(&mut graph, p);
        let block = vec![add];
        let opts = SelectOptions {
            frame_homed: true,
            ..SelectOptions::default()
        };
        let s = select_block(&graph, &block, None, &opts);
        let t = s.tiles.iter().find(|t| t.root == add).expect("root");
        assert_eq!(
            t.rule,
            Rule::AluReg,
            "frame homing means there is no copy to avoid"
        );
        assert!(
            matches!(t.insts.as_slice(), [MInst::AluRR { .. }]),
            "and no `MInst::Move` prefix either: {:?}",
            t.insts
        );
    }

    /// Ranking is micro-ops first. A three-byte `MOV` plus a three-byte `ADD`
    /// is *more* bytes than a four-byte `LEA`, but the point of the ordering is
    /// that it would still lose on micro-ops even if it were shorter.
    #[test]
    fn the_cost_key_ranks_micro_ops_before_bytes() {
        let cheap = SeqCost {
            bytes: 12,
            uops: 1,
            latency: 1,
        };
        let dear = SeqCost {
            bytes: 2,
            uops: 2,
            latency: 1,
        };
        assert!(cheap.key() < dear.key());
        assert_eq!(
            SeqCost::of(Cost::new(3, 1, 1))
                .then(SeqCost::of(Cost::new(3, 1, 1)))
                .key(),
            (2, 6, 2)
        );
    }

    /// The generic lowering has to be the most expensive thing the model can
    /// name, or a rule that covers more nodes would never be preferred.
    #[test]
    fn the_generic_lowering_is_the_most_expensive_option() {
        let generic = SeqCost::of(GENERIC_COST);
        for i in [
            MInst::AluRR {
                op: Op::Add,
                ty: Ty::I64,
                dst: 0,
                lhs: 1,
                rhs: 2,
            },
            MInst::Lea {
                dst: 0,
                ty: Ty::I64,
                addr: IrAddr {
                    base: Some(1),
                    index: Some(2),
                    scale: 8,
                    disp: 64,
                },
            },
            MInst::CmpRR {
                ty: Ty::I64,
                lhs: 0,
                rhs: 1,
            },
        ] {
            assert!(
                i.cost().key() < generic.key(),
                "{i:?} must be cheaper than the generic lowering"
            );
        }
    }

    // ── compare / branch fusion ───────────────────────────────────────────

    /// Build `if (a <cc> b) …` with the compare feeding only the branch.
    fn cmp_branch_graph(cc: CmpOp, rhs_zero: bool) -> (Graph, NodeId, NodeId, Vec<NodeId>) {
        let mut graph = g();
        let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
        let a = graph.add(IrOp::Param(0), IrType::Int, vec![], None);
        let b = if rhs_zero {
            konst_i32(&mut graph, 0)
        } else {
            graph.add(IrOp::Param(1), IrType::Int, vec![], None)
        };
        let cmp = graph.add(IrOp::Cmp(cc), IrType::Int, vec![a, b], None);
        let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
        let block = if rhs_zero { vec![b, cmp] } else { vec![cmp] };
        (graph, cmp, iff, block)
    }

    #[test]
    fn a_compare_that_only_feeds_a_branch_is_fused_into_it() {
        let (graph, cmp, iff, block) = cmp_branch_graph(CmpOp::Lt, false);
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        assert!(s.covers(&block), "coverage: {s:?}");
        let t = s.tiles.last().expect("a terminator tile");
        assert_eq!(t.rule, Rule::CmpBranch);
        assert!(t.covered.contains(&cmp), "the compare is consumed");
        assert!(matches!(
            t.insts.as_slice(),
            [MInst::CmpRR { .. }, MInst::Jcc { cc: CmpOp::Lt, .. }]
        ));
        // No SETcc/MOVZX anywhere: that is the whole saving.
        assert!(!s
            .tiles
            .iter()
            .flat_map(|t| t.insts.iter())
            .any(|i| matches!(i, MInst::SetCc { .. })));
    }

    #[test]
    fn a_compare_against_zero_becomes_test() {
        let (graph, cmp, iff, block) = cmp_branch_graph(CmpOp::Ne, true);
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        assert!(s.covers(&block), "coverage: {s:?}");
        let t = s.tiles.last().expect("a terminator tile");
        assert_eq!(t.rule, Rule::TestZeroBranch);
        assert!(t.covered.contains(&cmp));
        assert!(matches!(
            t.insts.as_slice(),
            [
                MInst::TestRR { ty: Ty::I32, .. },
                MInst::Jcc { cc: CmpOp::Ne, .. }
            ]
        ));
        // `TEST r32, r32` is two bytes where `CMP r32, imm8` is three.
        assert_eq!(
            MInst::TestRR {
                ty: Ty::I32,
                reg: 0
            }
            .cost()
            .bytes,
            2
        );
    }

    /// A zero on the *left* means the operands swapped, so the condition has
    /// to be mirrored — not negated. `0 < x` is `x > 0`, never `x >= 0`.
    #[test]
    fn a_zero_on_the_left_mirrors_the_condition() {
        let mut graph = g();
        let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
        let zero = konst_i32(&mut graph, 0);
        let x = graph.add(IrOp::Param(0), IrType::Int, vec![], None);
        let cmp = graph.add(IrOp::Cmp(CmpOp::Lt), IrType::Int, vec![zero, x], None);
        let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
        let block = vec![zero, cmp];
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        let t = s.tiles.last().expect("terminator");
        assert!(
            matches!(
                t.insts.as_slice(),
                [MInst::TestRR { reg, .. }, MInst::Jcc { cc: CmpOp::Gt, .. }] if *reg == x
            ),
            "0 < x must become TEST x,x / JG, got {:?}",
            t.insts
        );
        // Mirroring is not negation.
        assert_eq!(mirror(CmpOp::Lt), CmpOp::Gt);
        assert_eq!(CmpOp::Lt.negate(), CmpOp::Ge);
    }

    /// A compare with a second consumer still has to produce its 0/1 value, so
    /// the branch may not consume it.
    #[test]
    fn a_compare_with_a_second_consumer_is_not_fused() {
        let (mut graph, cmp, iff, block) = cmp_branch_graph(CmpOp::Eq, false);
        let _other = graph.add(IrOp::Neg, IrType::Int, vec![cmp], None);

        // Without the encodability gate, the rule that fires is `CMP; SETcc`.
        let lax = select_block(
            &graph,
            &block,
            Some(iff),
            &SelectOptions {
                require_encodable: false,
                fold_loads: false,
                frame_homed: false,
            },
        );
        let own = lax.tiles.iter().find(|t| t.root == cmp).expect("cmp tile");
        assert_eq!(own.rule, Rule::CmpSetCc);
        assert!(own.insts.iter().any(|i| matches!(i, MInst::SetCc { .. })));

        // With it, `SETcc` has no row (its 8-bit destination needs a REX rule
        // the table does not state), so the compare falls back to the generic
        // lowering — which is byte-for-byte what `ir_lower` emits today.
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        assert!(s.covers(&block));
        let term = s.tiles.last().expect("terminator");
        assert_eq!(term.rule, Rule::TestBranch);
        assert!(!term.covered.contains(&cmp));
        let own = s.tiles.iter().find(|t| t.root == cmp).expect("cmp tile");
        assert_eq!(own.rule, Rule::Generic);
        assert!(s.notes.iter().any(|n| matches!(
            n,
            Note::Unencodable {
                rule: Rule::CmpSetCc,
                ..
            }
        )));
    }

    /// A compare the scheduler put in another block cannot be fused: the
    /// instructions in between would have destroyed the flags.
    #[test]
    fn an_out_of_block_compare_is_not_fused() {
        let (graph, cmp, iff, _block) = cmp_branch_graph(CmpOp::Eq, false);
        let block: Vec<NodeId> = Vec::new();
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        let term = s.tiles.last().expect("terminator");
        assert_eq!(term.rule, Rule::TestBranch);
        assert!(!term.covered.contains(&cmp));
    }

    // ── the load-folding gate ─────────────────────────────────────────────

    /// `[ctrl, mem, base, offset]` load / `[ctrl, mem, base, offset, value]`
    /// store over one base, with constant offsets so the alias model can prove
    /// (or refuse) disjointness.
    struct MemFixture {
        graph: Graph,
        load: NodeId,
        store: NodeId,
        add: NodeId,
        block: Vec<NodeId>,
    }

    fn mem_fixture(load_off: i64, store_off: i64) -> MemFixture {
        let mut graph = g();
        let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(IrOp::Proj(1), IrType::Memory, vec![start], None);
        let base = graph.add(IrOp::Param(0), IrType::Ref, vec![], None);
        let lo = graph.add(IrOp::Const(load_off), IrType::Int, vec![], None);
        let so = graph.add(IrOp::Const(store_off), IrType::Int, vec![], None);
        let val = graph.add(IrOp::Param(1), IrType::Int, vec![], None);
        let load = graph.add(
            IrOp::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, lo],
            None,
        );
        // The builder threads the token through the load, so the store's
        // slot-1 edge names it. That edge must NOT count as a value use.
        let store = graph.add(
            IrOp::Store(MemKind::Int),
            IrType::Void,
            vec![ctrl, load, base, so, val],
            None,
        );
        let other = graph.add(IrOp::Param(2), IrType::Int, vec![], None);
        let add = graph.add(IrOp::Add, IrType::Int, vec![other, load], None);
        let block = vec![load, store, add];
        MemFixture {
            graph,
            load,
            store,
            add,
            block,
        }
    }

    #[test]
    fn the_memory_token_edge_is_not_a_value_use() {
        let f = mem_fixture(1, 1);
        let uses = ValueUses::of(&f.graph);
        assert_eq!(
            uses.count(f.load),
            1,
            "the store's token edge must not count; only the Add reads the value"
        );
        assert!(uses.single_use(f.load));
    }

    /// The defect this gate exists to prevent: folding the load into the Add
    /// moves it past the store, so it would read the value the store *just
    /// wrote* instead of the one that was there.
    #[test]
    fn a_load_is_not_folded_across_an_aliasing_store() {
        let f = mem_fixture(1, 1);
        let ctx = ctx_of(&f.graph, &f.block);
        assert_eq!(
            may_fold_load(&ctx, f.load, f.add),
            Err(FoldRefusal::Intervening {
                between: f.store,
                reason: ReorderBlock::MayAlias
            })
        );
        let opts = SelectOptions {
            require_encodable: false,
            fold_loads: true,
            frame_homed: false,
        };
        let s = select_block(&f.graph, &f.block, None, &opts);
        assert!(
            !s.matched().any(|t| t.rule == Rule::AluFoldedLoad),
            "no fold may survive an aliasing store"
        );
        assert!(s.covers(&f.block));
    }

    /// A store the alias model proves disjoint does not block the fold.
    #[test]
    fn a_load_is_folded_across_a_provably_disjoint_store() {
        let f = mem_fixture(1, 2);
        let ctx = ctx_of(&f.graph, &f.block);
        assert_eq!(may_fold_load(&ctx, f.load, f.add), Ok(()));
        let opts = SelectOptions {
            require_encodable: false,
            fold_loads: true,
            frame_homed: false,
        };
        let s = select_block(&f.graph, &f.block, None, &opts);
        let t = s.tiles.iter().find(|t| t.root == f.add).expect("add tile");
        assert_eq!(t.rule, Rule::AluFoldedLoad);
        assert!(t.covered.contains(&f.load));
        assert!(s.covers(&f.block), "coverage: {s:?}");
        // The folded load must not also be emitted on its own.
        assert!(
            !s.tiles.iter().any(|t| t.root == f.load),
            "the load may not be a tile root as well as folded"
        );
    }

    #[test]
    fn a_multiply_used_load_is_not_folded() {
        let mut f = mem_fixture(1, 2);
        let _second = f.graph.add(IrOp::Neg, IrType::Int, vec![f.load], None);
        let ctx = ctx_of(&f.graph, &f.block);
        assert_eq!(
            may_fold_load(&ctx, f.load, f.add),
            Err(FoldRefusal::MultipleUses {
                load: f.load,
                uses: 2
            })
        );
    }

    #[test]
    fn a_safepoint_pinned_load_is_not_folded() {
        let mut f = mem_fixture(1, 2);
        f.graph.safepoints.push(SafepointSnapshot {
            bci: 0,
            locals: vec![f.load],
            stack: Vec::new(),
        });
        let ctx = ctx_of(&f.graph, &f.block);
        assert_eq!(
            may_fold_load(&ctx, f.load, f.add),
            Err(FoldRefusal::SafepointPinned(f.load))
        );
    }

    #[test]
    fn a_load_after_its_user_is_not_folded() {
        let f = mem_fixture(1, 2);
        // Present the block in an order where the load comes last.
        let block = vec![f.store, f.add, f.load];
        let ctx = ctx_of(&f.graph, &block);
        assert_eq!(
            may_fold_load(&ctx, f.load, f.add),
            Err(FoldRefusal::NotBefore {
                load: f.load,
                user: f.add
            })
        );
    }

    #[test]
    fn a_load_outside_the_block_is_not_folded() {
        let f = mem_fixture(1, 2);
        let block = vec![f.add];
        let ctx = ctx_of(&f.graph, &block);
        assert_eq!(
            may_fold_load(&ctx, f.load, f.add),
            Err(FoldRefusal::NotInBlock(f.load))
        );
    }

    /// A call between the load and its user is opaque: it may write anything.
    #[test]
    fn a_load_is_not_folded_across_a_call() {
        let mut f = mem_fixture(1, 2);
        let ctrl = f.graph.nodes[f.load as usize].inputs[0];
        let call = f.graph.add(
            IrOp::Call { info_ptr: 0 },
            IrType::Int,
            vec![ctrl, f.load],
            None,
        );
        let block = vec![f.load, call, f.add];
        let ctx = ctx_of(&f.graph, &block);
        assert!(matches!(
            may_fold_load(&ctx, f.load, f.add),
            Err(FoldRefusal::Intervening { between, .. }) if between == call
        ));
    }

    // ── coverage and fail-closed behaviour ────────────────────────────────

    /// The invariant everything else rests on. A node covered twice is
    /// computed twice; a node covered zero times is a *dropped instruction* —
    /// the failure mode a catch-all `_ => {}` produces.
    #[test]
    fn every_block_node_is_covered_exactly_once() {
        let mut graph = g();
        let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
        let p = param(&mut graph, 0);
        let q = param(&mut graph, 1);
        let k2 = konst(&mut graph, 2);
        let shl = bin(&mut graph, IrOp::Shl, q, k2);
        let k8 = konst(&mut graph, 8);
        let a1 = bin(&mut graph, IrOp::Add, p, shl);
        let a2 = bin(&mut graph, IrOp::Add, a1, k8);
        let k5 = konst(&mut graph, 5);
        let and = bin(&mut graph, IrOp::And, a2, k5);
        let d = graph.add(IrOp::Div, IrType::Long, vec![and, p], None);
        let zero = konst(&mut graph, 0);
        let cmp = graph.add(IrOp::Cmp(CmpOp::Ne), IrType::Int, vec![d, zero], None);
        let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
        let block = vec![k2, shl, k8, a1, a2, k5, and, d, zero, cmp];
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        assert!(s.covers(&block), "coverage failed: {s:#?}");
        // No node appears in two tiles.
        let mut all: Vec<NodeId> = s
            .tiles
            .iter()
            .flat_map(|t| t.covered.iter().copied())
            .collect();
        let before = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(before, all.len(), "a node was covered twice");
    }

    /// An op no rule matches must come back as a tile that *names* it.
    #[test]
    fn an_unmatched_node_becomes_a_generic_tile_naming_it() {
        let mut graph = g();
        let p = param(&mut graph, 0);
        let q = param(&mut graph, 1);
        let d = graph.add(IrOp::Div, IrType::Long, vec![p, q], None);
        let block = vec![d];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        assert_eq!(s.tiles.len(), 1);
        assert_eq!(s.tiles[0].rule, Rule::Generic);
        assert_eq!(s.tiles[0].insts, vec![MInst::Generic { node: d }]);
        assert!(s.covers(&block));
    }

    /// A `MonitorEnter` must never vanish. This is the shape of the bug the
    /// brief names: `lower_data_node`'s catch-all meant an unguarded monitor
    /// op compiled to nothing and a lock was dropped.
    #[test]
    fn an_unmatched_monitor_op_is_never_silently_dropped() {
        let mut graph = g();
        let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(IrOp::Proj(1), IrType::Memory, vec![start], None);
        let obj = graph.add(IrOp::Param(0), IrType::Ref, vec![], None);
        let enter = graph.add(
            IrOp::MonitorEnter,
            IrType::Memory,
            vec![ctrl, mem, obj],
            None,
        );
        let exit = graph.add(
            IrOp::MonitorExit,
            IrType::Memory,
            vec![ctrl, enter, obj],
            None,
        );
        let block = vec![enter, exit];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        assert!(s.covers(&block), "both monitor ops must be covered");
        for id in [enter, exit] {
            let t = s.tiles.iter().find(|t| t.root == id).expect("a tile");
            assert_eq!(t.rule, Rule::Generic);
            assert_eq!(t.insts, vec![MInst::Generic { node: id }]);
        }
    }

    /// A terminator always gets a tile. A branch selection declined to cover
    /// is a branch nobody emits — the one failure this design must not have.
    #[test]
    fn a_terminator_always_gets_a_tile() {
        // (a) an `If` whose compare is fusable.
        let (graph, _cmp, iff, block) = cmp_branch_graph(CmpOp::Le, false);
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        assert!(matches!(
            s.tiles.last().map(|t| t.root),
            Some(r) if r == iff
        ));

        // (b) a `Return`, which no rule matches: still a tile, and it NAMES
        // the node so the caller knows it has to emit the epilogue.
        let mut graph = g();
        let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
        let ret = graph.add(IrOp::Return, IrType::Control, vec![ctrl], None);
        let s = select_block(&graph, &[], Some(ret), &SelectOptions::default());
        assert_eq!(s.tiles.len(), 1);
        assert_eq!(s.tiles[0].rule, Rule::Generic);
        assert_eq!(s.tiles[0].insts, vec![MInst::Generic { node: ret }]);
    }

    /// A tile the pattern table cannot encode is discarded, not emitted.
    #[test]
    fn a_tile_the_table_cannot_encode_is_discarded_and_reported() {
        let mut graph = g();
        let p = param(&mut graph, 0);
        let k = konst(&mut graph, 5);
        // `XOR r64, imm8` has no row.
        let x = bin(&mut graph, IrOp::Xor, p, k);
        let block = vec![k, x];

        let lax = select_block(
            &graph,
            &block,
            None,
            &SelectOptions {
                require_encodable: false,
                fold_loads: false,
                frame_homed: false,
            },
        );
        assert_eq!(
            lax.tiles.iter().find(|t| t.root == x).map(|t| t.rule),
            Some(Rule::AluImm),
            "without the gate the immediate form is what the rules pick"
        );

        let strict = select_block(&graph, &block, None, &SelectOptions::default());
        let t = strict.tiles.iter().find(|t| t.root == x).expect("root");
        assert_eq!(t.rule, Rule::AluReg, "the gate must fall back, not emit");
        assert!(
            strict.notes.iter().any(|n| matches!(
                n,
                Note::Unencodable {
                    rule: Rule::AluImm,
                    ..
                }
            )),
            "the discard must be reported: {:?}",
            strict.notes
        );
        assert!(strict.covers(&block));
    }

    /// Under the default options every non-generic instruction the selector
    /// chooses has a proven encoding. This is what makes `require_encodable`
    /// a gate rather than a comment.
    #[test]
    fn every_selected_instruction_encodes_under_the_default_options() {
        let mut graph = g();
        let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
        let p = param(&mut graph, 0);
        let q = param(&mut graph, 1);
        let k2 = konst(&mut graph, 2);
        let shl = bin(&mut graph, IrOp::Shl, q, k2);
        let a1 = bin(&mut graph, IrOp::Add, p, shl);
        let k = konst(&mut graph, 40);
        let a2 = bin(&mut graph, IrOp::Add, a1, k);
        let _live = second_use(&mut graph, a2);
        let zero = konst(&mut graph, 0);
        let cmp = graph.add(IrOp::Cmp(CmpOp::Gt), IrType::Int, vec![a2, zero], None);
        let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
        let block = vec![k2, shl, a1, k, a2, zero, cmp];
        let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
        assert!(s.covers(&block), "coverage: {s:#?}");
        for t in s.matched() {
            for i in &t.insts {
                let e = i
                    .probe()
                    .unwrap_or_else(|e| panic!("{i:?} in {:?} does not encode: {e}", t.rule));
                assert!(!e.bytes.is_empty(), "{i:?} encoded to nothing");
            }
        }
    }

    /// Selection is a function of its input: the same graph and block must
    /// produce the same tiling every time, or a regression cannot be bisected.
    #[test]
    fn selection_is_deterministic() {
        let mut graph = g();
        let p = param(&mut graph, 0);
        let q = param(&mut graph, 1);
        let k2 = konst(&mut graph, 2);
        let shl = bin(&mut graph, IrOp::Shl, q, k2);
        let a1 = bin(&mut graph, IrOp::Add, p, shl);
        let block = vec![k2, shl, a1];
        let a = select_block(&graph, &block, None, &SelectOptions::default());
        let b = select_block(&graph, &block, None, &SelectOptions::default());
        assert_eq!(a, b);
    }

    /// Every 32-bit immediate row reproduces the `x64.rs` byte literal it
    /// names, exactly.
    ///
    /// The table's one trustworthy property is that a row is anchored to
    /// hand-written code it matches byte-for-byte; eight rows added on the
    /// strength of a coverage measurement are eight chances to weaken it. Each
    /// literal below is copied from the constant-folding fast path in `x64.rs`
    /// (the `iadd`/`isub`/`iand`/`ior`/`ixor`/`if_icmp` const arms), with EAX
    /// as the destination — which is what those arms use, and why no REX
    /// prefix appears.
    ///
    /// The exact edit that trips it: change any `RegF::Ext(n)` above. The
    /// `/n` digit is the opcode extension that distinguishes `ADD` from `SUB`
    /// from `AND` in the shared `0x83` group, and getting it wrong produces a
    /// valid instruction that computes something else entirely.
    #[test]
    fn the_32bit_immediate_rows_reproduce_the_x64_byte_literals() {
        // `x64.rs` constant-folding fast path, EAX destination — which is why
        // no REX prefix appears in any of these.
        assert_eq!(
            sel(&gpr_imm(Op::Add, Ty::I32, RAX, 7)),
            vec![0x83, 0xC0, 0x07]
        );
        assert_eq!(
            sel(&gpr_imm(Op::Add, Ty::I32, RAX, 100_000)),
            vec![0x81, 0xC0, 0xA0, 0x86, 0x01, 0x00]
        );
        assert_eq!(
            sel(&gpr_imm(Op::Sub, Ty::I32, RAX, 7)),
            vec![0x83, 0xE8, 0x07]
        );
        assert_eq!(
            sel(&gpr_imm(Op::Sub, Ty::I32, RAX, 100_000)),
            vec![0x81, 0xE8, 0xA0, 0x86, 0x01, 0x00]
        );
        assert_eq!(
            sel(&gpr_imm(Op::And, Ty::I32, RAX, 7)),
            vec![0x83, 0xE0, 0x07]
        );
        assert_eq!(
            sel(&gpr_imm(Op::Or, Ty::I32, RAX, 7)),
            vec![0x83, 0xC8, 0x07]
        );
        assert_eq!(
            sel(&gpr_imm(Op::Xor, Ty::I32, RAX, 7)),
            vec![0x83, 0xF0, 0x07]
        );
        assert_eq!(
            sel(&gpr_imm(Op::Cmp, Ty::I32, RAX, 7)),
            vec![0x83, 0xF8, 0x07]
        );
    }

    /// `lea_r32_m` reproduces the three byte literals `emit_imul_const` emits.
    ///
    /// `Rule::Lea` fired **zero** times on 850 real Spring Boot compiles
    /// because `tile_lea` refused every `Ty::I32` root — Java arithmetic is
    /// 32-bit and the table had only the REX.W form. This is the anchor for the
    /// row that unblocked it: `x64/arith.rs:790`, `:797`, `:804` already write
    /// exactly these bytes for `imul` by 3, 5 and 9.
    ///
    /// The exact edit that trips it: set `rex_w: true` on the row, or change
    /// `RexMode::OnDemand` to `RexMode::Always`. Either adds a `0x48` and the
    /// row stops being the literal it claims.
    #[test]
    fn the_32bit_lea_row_reproduces_the_imul_const_byte_literals() {
        for (scale, want) in [
            (2u8, vec![0x8Du8, 0x04, 0x40]),
            (4, vec![0x8D, 0x04, 0x80]),
            (8, vec![0x8D, 0x04, 0xC0]),
        ] {
            let mem = Mem {
                base: RAX,
                index: Some(Index { reg: RAX, scale }),
                disp: 0,
                force_disp32: false,
            };
            let got = sel(&Req::new(
                Op::Lea,
                Ty::I32,
                Operand::Gpr(RAX),
                Operand::Mem(mem),
            ));
            assert_eq!(got, want, "LEA EAX, [RAX + RAX*{scale}]");
        }
    }

    /// An `int` add/shift/multiply tree becomes one `LEA`, which is the whole
    /// point of the row above: before it, `tile_lea` returned `None` for every
    /// `IrType::Int` root and the rule was dead on real code.
    #[test]
    fn an_int_address_tree_now_tiles_as_a_lea() {
        let mut graph = g();
        let p = graph.add(IrOp::Param(0), IrType::Int, vec![], None);
        let q = graph.add(IrOp::Param(1), IrType::Int, vec![], None);
        let k = konst_i32(&mut graph, 2);
        let shl = graph.add(IrOp::Shl, IrType::Int, vec![q, k], None);
        let add = graph.add(IrOp::Add, IrType::Int, vec![p, shl], None);
        let block = vec![k, shl, add];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        assert!(s.covers(&block), "coverage: {s:?}");
        let root = s.tiles.iter().find(|t| t.root == add).expect("root");
        assert_eq!(root.rule, Rule::Lea, "an int address tree must fold");
        match root.insts.as_slice() {
            [MInst::Lea { ty, addr, .. }] => {
                assert_eq!(*ty, Ty::I32, "an int root must select the 32-bit form");
                assert_eq!(addr.scale, 4);
            }
            other => panic!("expected one LEA, got {other:?}"),
        }
        // And the tile the selector produced is one the table can encode —
        // a rule that fires but cannot encode is discarded by
        // `require_encodable` and reads as "fired zero times" all over again.
        for i in &root.insts {
            i.probe().unwrap_or_else(|e| panic!("{i:?}: {e:?}"));
        }
    }

    /// The new rows are reachable through `MInst::probe`, not merely present.
    ///
    /// A row the table has but `pattern_name` cannot name is a row
    /// `SelectOptions::require_encodable` still discards — which is the state
    /// the whole 850-method measurement was taken in. Both halves or neither.
    #[test]
    fn a_32bit_immediate_tile_is_encodable() {
        for (op, form, imm) in [
            (Op::Add, ImmForm::Imm8, 7i64),
            (Op::Add, ImmForm::Imm32, 100_000),
            (Op::Sub, ImmForm::Imm8, 7),
            (Op::And, ImmForm::Imm8, 7),
            (Op::Or, ImmForm::Imm8, 7),
            (Op::Xor, ImmForm::Imm8, 7),
        ] {
            let inst = MInst::AluRI {
                op,
                ty: Ty::I32,
                dst: 0,
                lhs: 1,
                imm,
                form,
            };
            assert!(
                inst.pattern_name().is_some(),
                "{op:?}/{form:?} at I32 has a row but no `pattern_name` mapping"
            );
            inst.probe()
                .unwrap_or_else(|e| panic!("{op:?}/{form:?} at I32: {e:?}"));
        }
    }

    /// The instructions the table cannot encode yet, pinned so the gap is a
    /// fact rather than a surprise. Shrinking this list is the next wave's
    /// work; growing it silently is what this test prevents.
    #[test]
    fn the_rows_the_selector_still_lacks_are_exactly_these() {
        let missing: Vec<&'static str> = [
            MInst::AluRM {
                op: Op::Add,
                ty: Ty::I64,
                dst: 0,
                lhs: 1,
                addr: AddrSource::Opaque(2),
                load: 2,
            },
            MInst::CmpRI {
                ty: Ty::I64,
                lhs: 0,
                imm: 1,
                form: ImmForm::Imm8,
            },
            MInst::SetCc {
                dst: 0,
                cc: CmpOp::Eq,
            },
            MInst::AluRI {
                op: Op::Xor,
                ty: Ty::I64,
                dst: 0,
                lhs: 1,
                imm: 1,
                form: ImmForm::Imm8,
            },
        ]
        .iter()
        .filter(|i| i.pattern_name().is_none())
        .map(|_| "unencodable")
        .collect();
        assert_eq!(
            missing.len(),
            4,
            "these four instruction shapes have no PATTERNS row; see \
             docs/jit/instruction-selection.md"
        );
        // And the ones that DO have rows really resolve to a row.
        for i in [
            MInst::AluRR {
                op: Op::Add,
                ty: Ty::I64,
                dst: 0,
                lhs: 1,
                rhs: 2,
            },
            MInst::Move {
                dst: 0,
                ty: Ty::I64,
                src: 1,
            },
            MInst::Lea {
                dst: 0,
                ty: Ty::I64,
                addr: IrAddr {
                    base: Some(1),
                    index: None,
                    scale: 1,
                    disp: 0,
                },
            },
            MInst::Lea {
                dst: 0,
                ty: Ty::I32,
                addr: IrAddr {
                    base: Some(1),
                    index: Some(1),
                    scale: 2,
                    disp: 0,
                },
            },
            MInst::TestRR {
                ty: Ty::I32,
                reg: 0,
            },
            MInst::Jcc {
                cc: CmpOp::Ne,
                at: 0,
            },
        ] {
            let name = i.pattern_name().unwrap_or_else(|| panic!("{i:?}"));
            assert!(pattern(name).is_some(), "`{name}` is not in PATTERNS");
            assert!(i.probe().is_ok(), "{i:?} must encode");
        }
    }
}
