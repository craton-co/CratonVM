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

use super::{base_requires_sib, is_extended, rex, Disp, DispOutOfRange};
use super::{GPR64_NAMES, RAX, RCX, RDX, RSP, XMM_NAMES};

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
}

impl std::fmt::Display for SelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelError::Constraint {
                pattern,
                constraint,
            } => write!(f, "pattern `{pattern}` rejects the operands: {constraint:?}"),
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
        self.modrm.map(|m| (self.rex_bit(0x04) << 3) | ((m >> 3) & 7))
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x64::{RAX, RBP, RBX, RCX, RDI, RDX, RSI, RSP};
    use crate::x64::{R11, R12, R13, R15, R8, R9};

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
                assert_eq!(b & 7, 0, "`{}`: opcode {b:#04X} has no register room", p.name);
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
                    panic!("`{}` could not encode its canonical operands: {err}", p.name)
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
                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_rsp_disp_from_reg(disp, reg)
                });
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
                    let rm = |op: Op, ty: Ty| {
                        Req::new(op, ty, Operand::Gpr(dst), Operand::Mem(mem))
                    };

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
                        assert_eq!(sel(&rm(op, ty)), want, "MOV{}X {bits}", if signed { "S" } else { "Z" });
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
                    let got = sel(&gpr(Op::Alu, Ty::I32, dst, src)
                        .with_extra(Operand::OpByte(opcode)));
                    assert_eq!(got, want, "ALU {opcode:#04X} r32");
                }

                for cc in [0x44u8, 0x45, 0x4C, 0x4D, 0x4E, 0x4F] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_cmov_cc_reg_reg(cc, dst, src)
                    });
                    let got =
                        sel(&gpr(Op::Cmov, Ty::I64, dst, src).with_extra(Operand::Cc(cc)));
                    assert_eq!(got, want, "CMOV{cc:#04X}");
                }
            }
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r64_r64(dst));
            assert_eq!(sel(&gpr(Op::Test, Ty::I64, dst, dst)), want, "TEST r64");

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r32_r32(dst));
            assert_eq!(sel(&gpr(Op::Test, Ty::I32, dst, dst)), want, "TEST r32");

            for imm in [0i8, 1, -1, 7, -8, i8::MAX, i8::MIN] {
                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_add_r64_imm8(dst, imm));
                assert_eq!(
                    sel(&gpr_imm(Op::Add, Ty::I64, dst, i64::from(imm))),
                    want,
                    "ADD r{dst}, {imm}"
                );

                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_and_r64_imm8(dst, imm));
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
                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_test_r64_imm32(dst, imm));
                assert_eq!(
                    sel(&gpr_imm(Op::Test, Ty::I64, dst, i64::from(imm))),
                    want,
                    "TEST r{dst}, {imm}"
                );
            }

            for shift in [0u8, 1, 3, 32, 63] {
                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_shr_r64_imm8(dst, shift));
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
                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_movsd_xmm_xmm(xmm, src));
                let got = sel(&Req::new(
                    Op::Movsd,
                    Ty::F64,
                    Operand::Xmm(xmm),
                    Operand::Xmm(src),
                ));
                assert_eq!(got, want, "MOVSD xmm{xmm}, xmm{src}");

                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_movss_xmm_xmm(xmm, src));
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
            sel(&Req::new(Op::Shl, Ty::I32, Operand::Gpr(RAX), Operand::None)),
            vec![0xD3, 0xE0]
        );
        assert_eq!(
            sel(&Req::new(Op::Sar, Ty::I32, Operand::Gpr(RAX), Operand::None)),
            vec![0xD3, 0xF8]
        );
        assert_eq!(
            sel(&Req::new(Op::Shr, Ty::I32, Operand::Gpr(RAX), Operand::None)),
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
            sel(&Req::new(Op::Idiv, Ty::I64, Operand::Gpr(RCX), Operand::None)),
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
            sel(&Req::new(Op::Idiv, Ty::I32, Operand::Gpr(RCX), Operand::None)),
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
            0, 1, -1, 8, 127, -128, 128, -129, 1024, -1024, 65_536, i64::from(i32::MAX),
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
                    assert_eq!(
                        d.base(),
                        Some(base),
                        "`{}` lost the base register",
                        p.name
                    );
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
        for v in [1i64, -1, 127, -128, 128, i64::from(i32::MAX), i64::from(i32::MIN)] {
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
                    assert_eq!(d.len, e.bytes.len(), "`{}` decoder length", p.name);

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
}
