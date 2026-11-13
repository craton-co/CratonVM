// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The kernel instruction list, and the target-neutral instructions in
//! it.
//!
//! # Why this exists
//!
//! `docs/gpu/lowering-architecture.md` describes this crate as a second
//! Java bytecode front-end that "emits target text directly, with no
//! intermediate representation", and proposes the fix under **"Smaller:
//! an instruction list instead of a `String`"**: give the emitter a
//! `Vec` of instructions and render text once at the end. A second
//! target — AMD HIP C++ — is what forced it, because a `String` of PTX
//! can be rendered to exactly one thing.
//!
//! # How this lands without a 194-site big bang
//!
//! [`Instr::Raw`] is the hinge. It holds one already-rendered PTX line,
//! so the emitter's existing `writeln!(self.body, ...)` calls keep
//! working unchanged — [`Body`] implements [`std::fmt::Write`] and
//! turns each line into a `Raw`. The PTX renderer emits a `Raw`
//! verbatim, which is what keeps `tests/golden/ptx/` byte-identical
//! through the whole migration.
//!
//! Every other target **refuses** a `Raw`. So the set of instructions
//! expressed structurally is exactly the set a non-PTX backend can
//! lower, the refusal is loud and specific, and a kernel is never
//! translated on a guess. Converting a family of emission sites to a
//! typed variant is what widens HIP coverage, one family at a time,
//! with the goldens proving each step changed no PTX.
//!
//! That is the same discipline the analyzer already runs on: refuse
//! what cannot be lowered, and let the accepted set grow deliberately.
//!
//! # What is deliberately not here
//!
//! Register ALLOCATION. `RegPool` keeps its per-kind counters and its
//! numbering, because that numbering is what the golden PTX records —
//! changing it would make every golden move for no behavioural reason
//! and destroy the one signal this migration has.

use std::fmt;

mod lift;

/// A virtual register, named the way PTX names it.
///
/// Carried as the already-formatted name rather than a `(kind, number)`
/// pair for the duration of the migration: `emit.rs` compares register
/// names in five places (`IndexProof`, `param_ptr_reg`,
/// `is_reserved_reg`, `param_len_guarded`, `copy_reg`'s early-out) and
/// splitting identity from spelling is a separate change with its own
/// risk. A renderer that needs a different spelling strips the `%`.
pub type RegName = str;

/// One instruction in a kernel body.
///
/// The variants that carry structure are the ones a non-PTX target can
/// render; [`Instr::Raw`] is the tail that has not been converted yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Instr {
    /// One rendered PTX line, including its trailing newline.
    ///
    /// Produced by [`Body`]'s `fmt::Write` impl, so every
    /// `writeln!(self.body, ...)` in `emit.rs` still works. The PTX
    /// renderer writes it out unchanged; every other renderer refuses
    /// it by name, because translating PTX text on a guess is how a
    /// kernel comes to return plausible wrong answers.
    Raw(String),

    /// `mov.u32 %dst, %tid.x;` and friends — a read of a launch-shape
    /// special register.
    ReadSpecial { dst: String, which: SpecialReg },

    /// A label, without the trailing colon.
    Label(String),

    /// `bra L;` or `@%p bra L;` / `@!%p bra L;`.
    Bra {
        target: String,
        pred: Option<PredUse>,
    },

    /// `ret;`
    Ret,

    /// `ld.param.<ty> %dst, [<param>];` — a kernel parameter read.
    LdParam {
        dst: String,
        ty: Ty,
        param: ParamRef,
    },

    /// `mov.<ty> %dst, <imm>;` — a literal into a register.
    MovImm { dst: String, ty: Ty, imm: Imm },

    /// `mov.b<width> %dst, %src;` — a reinterpret, not a conversion.
    ///
    /// The emitter uses this to move between signed and unsigned
    /// registers of the same width, which in C++ is a cast and in PTX
    /// is a bit move. Kept distinct from [`Instr::Mov`] so a renderer
    /// that needs the cast can tell the two apart.
    Bitcast {
        dst: String,
        src: String,
        width: u32,
    },

    /// `mov.<ty> %dst, %src;` — a register copy.
    Mov { dst: String, ty: Ty, src: String },

    /// `<op>.<ty> %dst, %a, %b;` — a two-operand arithmetic or logical
    /// instruction.
    Bin {
        op: BinOp,
        ty: Ty,
        dst: String,
        a: Operand,
        b: Operand,
    },

    /// `mad.lo.<ty> %dst, %a, %b, %c;` — the low half of `a * b + c`.
    ///
    /// Not a `Bin` pair: it is one instruction on both targets, and
    /// splitting it would invite a renderer to contract the halves back
    /// into an FMA — which for float rounds once where Java rounds
    /// twice.
    MadLo {
        ty: Ty,
        dst: String,
        a: String,
        b: String,
        c: String,
    },

    /// `mad.wide.s32 %dst, %index, <elem_size>, %base;` — the address of
    /// element `index` of the array based at `base`.
    ///
    /// One node rather than a widening multiply plus an add, because
    /// that is what it MEANS, and because `param` then tells a
    /// C-flavoured renderer the pointee type without a dataflow pass.
    /// The widening of the index is not decoration: it is what `.wide`
    /// does, and without it a large index overflows in 32 bits exactly
    /// where PTX would not.
    ElementAddr {
        dst: String,
        base: String,
        index: String,
        elem_size: u32,
        /// Which kernel parameter `base` is, so a renderer can recover
        /// the element type. `None` when the base is not a parameter
        /// pointer.
        param: Option<u32>,
    },

    /// `ld.global.<ty> %dst, [%addr];`
    LdGlobal { dst: String, ty: Ty, addr: String },

    /// `st.global.<ty> [%addr], %value;`
    StGlobal {
        ty: Ty,
        addr: String,
        value: Operand,
    },

    /// `setp.<cmp>.<ty> %dst, %a, %b;` — a comparison into a predicate.
    ///
    /// `ty` is the OPERATION's type and may differ from either
    /// operand's declared register kind: PTX's relaxed operand typing
    /// lets `setp.ge.u32` read two `.s32` registers, and the emitter
    /// relies on that for the unsigned bounds check. A renderer whose
    /// language cannot reinterpret for free has to insert the cast.
    Setp {
        cmp: Cmp,
        ty: Ty,
        dst: String,
        a: Operand,
        b: Operand,
    },

    /// `<add|sub|mul|div>.rn.<f32|f64> %dst, %a, %b;` -- IEEE float
    /// arithmetic with the rounding mode spelled out.
    ///
    /// Not a [`Instr::Bin`]: Java requires round-to-nearest-even for every
    /// float operation, PTX needs `.rn` to say so, and a renderer must not
    /// be free to treat `a * b + c` as one fused operation.
    FArith {
        op: FOp,
        ty: Ty,
        dst: String,
        a: Operand,
        b: Operand,
    },

    /// `<div|rem>.<s32|s64|u32|u64> %dst, %a, %b;` -- Java's `/` and `%`.
    ///
    /// Separate from [`Instr::Bin`] because the C++ operators differ from
    /// Java on exactly one input: `MIN_VALUE / -1` is undefined behaviour
    /// in C++ and `MIN_VALUE` in Java.
    IDivRem {
        rem: bool,
        ty: Ty,
        dst: String,
        a: Operand,
        b: Operand,
    },

    /// `neg.<ty>`, `abs.<ty>`, `sqrt.rn.<ty>`.
    Unary {
        op: UnOp,
        ty: Ty,
        dst: String,
        src: Operand,
    },

    /// `fma.rn.<f32|f64> %dst, %a, %b, %c;` -- a deliberately fused
    /// multiply-add (`Math.fma`), as opposed to the two roundings of
    /// `a * b + c`.
    Fma {
        ty: Ty,
        dst: String,
        a: Operand,
        b: Operand,
        c: Operand,
    },

    /// `cvt[.rn|.rzi].<dst_ty>.<src_ty> %dst, %src;`
    Cvt {
        mode: CvtMode,
        dst_ty: Ty,
        src_ty: Ty,
        dst: String,
        src: String,
    },

    /// `selp.<ty> %dst, %a, %b, %p;` -- `p ? a : b`.
    Selp {
        ty: Ty,
        dst: String,
        a: Operand,
        b: Operand,
        pred: String,
    },

    /// `and.pred` / `or.pred`.
    PredBin {
        op: PredOp,
        dst: String,
        a: String,
        b: String,
    },

    /// `red.global.add.<ty> [%addr], %value;` -- an atomic add whose old
    /// value is discarded.
    RedAdd { ty: Ty, addr: String, value: String },

    /// `@%p <inner>` / `@!%p <inner>` -- any non-branch instruction run
    /// only when the predicate holds. A guarded branch is [`Instr::Bra`].
    Guarded { pred: PredUse, inner: Box<Instr> },

    /// Fold a warp's contributions to `acc` into lane 0 with a
    /// shuffle-down tree and `+`.
    ///
    /// Structural because the PTX spelling hard-codes five steps of a
    /// 32-lane warp, and an AMD wavefront is 64 lanes on CDNA: the same
    /// five steps there drop half of every reduction and return a
    /// plausible number. `ptx` is the already-numbered PTX for this
    /// target -- it carries the temporaries' register names so the PTX
    /// stays byte-identical -- and `acc`/`ty` are what another target
    /// needs to expand the fold for ITS lane count.
    WarpFold { acc: String, ty: Ty, ptx: String },
}

/// Which kernel parameter an [`Instr::LdParam`] reads.
///
/// Structured rather than a formatted name so a renderer can tell an
/// array pointer from its length from a scalar, which is what it needs
/// in order to give each one a type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamRef {
    /// `p<N>_ptr` — the base address of array parameter `N`.
    ArrayPtr(u32),
    /// `p<N>_len` — the length of array parameter `N`.
    ArrayLen(u32),
    /// `p<N>` — a scalar parameter.
    Scalar(u32),
    /// `failure_flag` — the deopt signal the bounds and div-by-zero
    /// guards store into.
    FailureFlag,
    /// `tid_base` — the chunk offset added to the thread index, because
    /// a launch has no index offset of its own.
    TidBase,
    /// `ret_ptr` — where a reduction kernel accumulates its result.
    RetPtr,
    /// `ret_len` — the length of the return buffer.
    RetLen,
}

impl ParamRef {
    /// The `.param` name, which is identical on every target: it is the
    /// kernel's own parameter list, built once by
    /// `lowering::build_param_list`.
    pub fn name(&self) -> String {
        match self {
            ParamRef::ArrayPtr(i) => format!("p{i}_ptr"),
            ParamRef::ArrayLen(i) => format!("p{i}_len"),
            ParamRef::Scalar(i) => format!("p{i}"),
            ParamRef::FailureFlag => "failure_flag".to_string(),
            ParamRef::TidBase => "tid_base".to_string(),
            ParamRef::RetPtr => "ret_ptr".to_string(),
            ParamRef::RetLen => "ret_len".to_string(),
        }
    }
}

/// An instruction operand: a register or an immediate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operand {
    Reg(String),
    Imm(Imm),
}

impl Operand {
    fn ptx(&self) -> String {
        match self {
            Operand::Reg(r) => r.clone(),
            Operand::Imm(i) => i.ptx(),
        }
    }
}

/// A literal, carrying the spelling PTX recorded it in.
///
/// The spelling is not cosmetic. The emitter writes `0x1f`,
/// `0xffffffff`, `0f3FB8AA3B` and plain decimals, and the goldens
/// recorded exactly those — so an immediate that re-formats itself
/// would move every golden for no behavioural reason and destroy the
/// signal this migration depends on. A renderer for another language
/// reads [`Imm::bits`] and ignores the spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Imm {
    /// A decimal integer, as written.
    Int(i64),
    /// A hexadecimal integer, with the exact digits PTX recorded.
    Hex(String),
    /// A single-precision bit pattern as PTX spells it: `0f` and eight
    /// hex digits (`0f3FB8AA3B`).
    F32(String),
    /// A double-precision bit pattern: `0d` and sixteen hex digits.
    F64(String),
}

impl Imm {
    fn ptx(&self) -> String {
        match self {
            Imm::Int(v) => v.to_string(),
            Imm::Hex(h) | Imm::F32(h) | Imm::F64(h) => h.clone(),
        }
    }

    /// The value, for a renderer that does not care how it was spelled.
    pub fn bits(&self) -> i64 {
        match self {
            Imm::Int(v) => *v,
            Imm::Hex(h) => {
                let t = h.trim_start_matches("0x").trim_start_matches("0X");
                i64::from_str_radix(t, 16).unwrap_or(0)
            }
            // The BIT PATTERN, not a numeric value: a float immediate is
            // reproduced exactly by a bit-cast, never by formatting a
            // decimal.
            Imm::F32(h) | Imm::F64(h) => u64::from_str_radix(&h[2..], 16).unwrap_or(0) as i64,
        }
    }
}

/// Which launch-shape register is being read.
///
/// These are the only three the emitter reads, and every target has an
/// exact equivalent — `threadIdx.x` / `blockDim.x` / `blockIdx.x` in
/// HIP C++. They are rendered LITERALLY on every target, including the
/// `mad.lo` that combines them: a target-specific "global id" helper
/// would fold in an offset the host never sets, and agreeing today is
/// not the same as depending on nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialReg {
    /// Lane within the block.
    ThreadIdxX,
    /// Threads per block.
    BlockDimX,
    /// Block within the grid.
    BlockIdxX,
    /// Lane within the warp / wavefront (`%laneid`).
    LaneId,
}

impl SpecialReg {
    /// The PTX special-register spelling.
    pub fn ptx(self) -> &'static str {
        match self {
            SpecialReg::ThreadIdxX => "%tid.x",
            SpecialReg::BlockDimX => "%ntid.x",
            SpecialReg::BlockIdxX => "%ctaid.x",
            SpecialReg::LaneId => "%laneid",
        }
    }

    /// The inverse of [`SpecialReg::ptx`].
    pub fn from_ptx(s: &str) -> Option<SpecialReg> {
        [
            SpecialReg::ThreadIdxX,
            SpecialReg::BlockDimX,
            SpecialReg::BlockIdxX,
            SpecialReg::LaneId,
        ]
        .into_iter()
        .find(|r| r.ptx() == s)
    }

    /// The HIP C++ (and CUDA C++) spelling.
    pub fn cpp(self) -> &'static str {
        match self {
            SpecialReg::ThreadIdxX => "threadIdx.x",
            SpecialReg::BlockDimX => "blockDim.x",
            SpecialReg::BlockIdxX => "blockIdx.x",
            SpecialReg::LaneId => "__lane_id()",
        }
    }
}

/// An instruction's type suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    S8,
    U8,
    S16,
    U16,
    S32,
    U32,
    S64,
    U64,
    /// IEEE half precision. Only ever a CONVERSION operand: a register
    /// holding half-precision bits is declared `.b16`, and every
    /// arithmetic instruction in this emitter works in `f32`.
    F16,
    F32,
    F64,
    B16,
    B32,
    B64,
}

impl Ty {
    pub fn ptx(self) -> &'static str {
        match self {
            Ty::S8 => "s8",
            Ty::U8 => "u8",
            Ty::S16 => "s16",
            Ty::U16 => "u16",
            Ty::S32 => "s32",
            Ty::U32 => "u32",
            Ty::S64 => "s64",
            Ty::U64 => "u64",
            Ty::F16 => "f16",
            Ty::F32 => "f32",
            Ty::F64 => "f64",
            Ty::B16 => "b16",
            Ty::B32 => "b32",
            Ty::B64 => "b64",
        }
    }

    /// The C++ spelling, for a HIP or CUDA renderer.
    ///
    /// `signed char` rather than `char` for [`Ty::S8`]: C++ leaves
    /// plain `char`'s signedness implementation-defined, and a `byte[]`
    /// element read as unsigned is wrong by 256 over half its range.
    pub fn cpp(self) -> &'static str {
        match self {
            Ty::S8 => "signed char",
            Ty::U8 => "unsigned char",
            Ty::S16 => "short",
            Ty::U16 | Ty::B16 => "unsigned short",
            Ty::S32 => "int",
            Ty::U32 | Ty::B32 => "unsigned int",
            Ty::S64 => "long long",
            Ty::U64 | Ty::B64 => "unsigned long long",
            Ty::F16 => "_Float16",
            Ty::F32 => "float",
            Ty::F64 => "double",
        }
    }
}

/// A two-operand arithmetic or logical operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Shl,
    Shr,
}

impl BinOp {
    /// The PTX mnemonic. Integer `mul` is `mul.lo`, because the
    /// emitter never wants the high half.
    pub fn ptx(self, ty: Ty) -> &'static str {
        let integer = !matches!(ty, Ty::F32 | Ty::F64);
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul if integer => "mul.lo",
            BinOp::Mul => "mul",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Xor => "xor",
            BinOp::Shl => "shl",
            BinOp::Shr => "shr",
        }
    }

    /// The C++ operator.
    pub fn cpp(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::And => "&",
            BinOp::Or => "|",
            BinOp::Xor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
        }
    }
}

/// A comparison predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `setp.nan`: true when EITHER operand is NaN. Not a C++ operator;
    /// a renderer must special-case it.
    Nan,
}

impl Cmp {
    /// The inverse of [`Cmp::ptx`].
    pub fn from_ptx(s: &str) -> Option<Cmp> {
        [
            Cmp::Eq,
            Cmp::Ne,
            Cmp::Lt,
            Cmp::Le,
            Cmp::Gt,
            Cmp::Ge,
            Cmp::Nan,
        ]
        .into_iter()
        .find(|c| c.ptx() == s)
    }

    pub fn ptx(self) -> &'static str {
        match self {
            Cmp::Eq => "eq",
            Cmp::Ne => "ne",
            Cmp::Lt => "lt",
            Cmp::Le => "le",
            Cmp::Gt => "gt",
            Cmp::Ge => "ge",
            Cmp::Nan => "nan",
        }
    }

    pub fn cpp(self) -> &'static str {
        match self {
            Cmp::Eq => "==",
            Cmp::Ne => "!=",
            Cmp::Lt => "<",
            Cmp::Le => "<=",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
            // Not an operator; see the variant's docs.
            Cmp::Nan => "!=",
        }
    }
}

/// The four float operations that carry an explicit `.rn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl FOp {
    pub fn from_ptx(s: &str) -> Option<FOp> {
        Some(match s {
            "add" => FOp::Add,
            "sub" => FOp::Sub,
            "mul" => FOp::Mul,
            "div" => FOp::Div,
            _ => return None,
        })
    }

    pub fn ptx(self) -> &'static str {
        match self {
            FOp::Add => "add",
            FOp::Sub => "sub",
            FOp::Mul => "mul",
            FOp::Div => "div",
        }
    }

    pub fn cpp(self) -> &'static str {
        match self {
            FOp::Add => "+",
            FOp::Sub => "-",
            FOp::Mul => "*",
            FOp::Div => "/",
        }
    }
}

/// A one-operand operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Abs,
    /// Always `sqrt.rn`.
    Sqrt,
    /// `ex2.approx.f32`: 2^x, NOT correctly rounded. Only emitted when
    /// `CRATONVM_GPU_APPROX_MATH=1` admitted `Math.exp`.
    Ex2Approx,
}

/// The rounding / saturation modifier on a `cvt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CvtMode {
    /// No modifier: the conversion is exact or an integer re-width.
    Exact,
    /// `.rn`: round to nearest even.
    Rn,
    /// `.rzi`: round toward zero to an integer.
    Rzi,
}

impl CvtMode {
    fn ptx(self) -> &'static str {
        match self {
            CvtMode::Exact => "",
            CvtMode::Rn => ".rn",
            CvtMode::Rzi => ".rzi",
        }
    }
}

/// A predicate-to-predicate operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredOp {
    And,
    Or,
}

/// A predicate guarding an instruction: `@%p` or `@!%p`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredUse {
    /// The predicate register's name, `%p`-prefixed as PTX spells it.
    pub reg: String,
    /// `@!%p` rather than `@%p`.
    pub negated: bool,
}

/// A kernel body under construction: the instruction list, plus the
/// line buffer that lets `writeln!` keep working.
///
/// `Default` is the empty body, which is what `std::mem::take` needs —
/// `try_emit_if_converted` speculates by taking the body, emitting into
/// a fresh one, and swapping back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Body {
    code: Vec<Instr>,
    /// Partial line from a `write!` that has not reached its newline.
    ///
    /// Always empty between statements in practice — every emission
    /// site is a `writeln!` — but `fmt::Write` may deliver one line
    /// across several `write_str` calls, so the buffer is not optional.
    pending: String,
}

impl fmt::Write for Body {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for part in s.split_inclusive('\n') {
            self.pending.push_str(part);
            if self.pending.ends_with('\n') {
                let line = std::mem::take(&mut self.pending);
                self.code.push(Instr::lift(line));
            }
        }
        Ok(())
    }
}

impl Body {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of instructions. Used as the splice point for the bounds
    /// prologue, which is why it counts instructions rather than bytes:
    /// the old code recorded a byte offset into a `String` and spliced
    /// text at it.
    pub fn len(&self) -> usize {
        debug_assert!(
            self.pending.is_empty(),
            "an instruction index was taken mid-line; every emission site \
             should be a `writeln!`"
        );
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty() && self.pending.is_empty()
    }

    pub fn code(&self) -> &[Instr] {
        &self.code
    }

    /// Append a structured instruction.
    pub fn push(&mut self, instr: Instr) {
        debug_assert!(
            self.pending.is_empty(),
            "a structured instruction was pushed mid-line"
        );
        self.code.push(instr);
    }

    /// Append one already-rendered PTX line, lifting it to a structured
    /// instruction when it has one. What `writeln!` does, for a caller
    /// that holds the line as a `String`.
    pub fn push_text(&mut self, line: String) {
        debug_assert!(self.pending.is_empty(), "a line was pushed mid-line");
        self.code.push(Instr::lift(line));
    }

    /// Append every instruction of `other`.
    pub fn append(&mut self, mut other: Body) {
        debug_assert!(other.pending.is_empty(), "appended a body mid-line");
        self.code.append(&mut other.code);
    }

    /// Insert `other`'s instructions at index `at`.
    ///
    /// The bounds preconditions `prove_index_within_param` emits are
    /// discovered while walking the body but have to EXECUTE before it,
    /// so they are spliced back to the point the dispatch guard
    /// recorded.
    pub fn splice_at(&mut self, at: usize, other: Body) {
        self.code.splice(at..at, other.code);
    }

    /// Whether any branch was emitted at or after `at`.
    ///
    /// Replaces `self.body[at..].contains("bra ")`. The `Raw` arm still
    /// reads text, because a `Raw` IS text — but it reads it in one
    /// place, on a value that knows it is an instruction, instead of
    /// scanning a `String` that has no idea where instruction
    /// boundaries are.
    pub fn has_branch_since(&self, at: usize) -> bool {
        if at > self.code.len() {
            return true;
        }
        self.code[at..].iter().any(|i| match i {
            Instr::Bra { .. } => true,
            Instr::Raw(t) => t.contains("bra "),
            _ => false,
        })
    }

    /// Render every instruction as PTX.
    ///
    /// Byte-identical to what the `String` body used to hold. That is
    /// the acceptance criterion for this whole migration and
    /// `tests/golden/ptx/` is the proof.
    pub fn render_ptx(&self) -> String {
        let mut out = String::with_capacity(self.code.len() * 32);
        for i in &self.code {
            render_ptx_instr(i, &mut out);
        }
        out.push_str(&self.pending);
        out
    }
}

/// One instruction as PTX text.
pub(crate) fn render_one(i: &Instr) -> String {
    let mut s = String::new();
    render_ptx_instr(i, &mut s);
    s
}

fn render_ptx_instr(i: &Instr, out: &mut String) {
    match i {
        Instr::Raw(t) => out.push_str(t),
        Instr::ReadSpecial { dst, which } => {
            out.push_str("    mov.u32 ");
            out.push_str(dst);
            out.push_str(", ");
            out.push_str(which.ptx());
            out.push_str(";\n");
        }
        Instr::Label(name) => {
            out.push_str(name);
            out.push_str(":\n");
        }
        Instr::Bra { target, pred } => {
            out.push_str("    ");
            if let Some(p) = pred {
                out.push('@');
                if p.negated {
                    out.push('!');
                }
                out.push_str(&p.reg);
                out.push(' ');
            }
            out.push_str("bra ");
            out.push_str(target);
            out.push_str(";\n");
        }
        Instr::Ret => out.push_str("    ret;\n"),
        Instr::LdParam { dst, ty, param } => {
            out.push_str(&format!(
                "    ld.param.{} {dst}, [{}];\n",
                ty.ptx(),
                param.name()
            ));
        }
        Instr::MovImm { dst, ty, imm } => {
            out.push_str(&format!("    mov.{} {dst}, {};\n", ty.ptx(), imm.ptx()));
        }
        Instr::Bitcast { dst, src, width } => {
            out.push_str(&format!("    mov.b{width} {dst}, {src};\n"));
        }
        Instr::Mov { dst, ty, src } => {
            out.push_str(&format!("    mov.{} {dst}, {src};\n", ty.ptx()));
        }
        Instr::Bin { op, ty, dst, a, b } => {
            out.push_str(&format!(
                "    {}.{} {dst}, {}, {};\n",
                op.ptx(*ty),
                ty.ptx(),
                a.ptx(),
                b.ptx()
            ));
        }
        Instr::MadLo { ty, dst, a, b, c } => {
            out.push_str(&format!("    mad.lo.{} {dst}, {a}, {b}, {c};\n", ty.ptx()));
        }
        Instr::ElementAddr {
            dst,
            base,
            index,
            elem_size,
            ..
        } => {
            out.push_str(&format!(
                "    mad.wide.s32 {dst}, {index}, {elem_size}, {base};\n"
            ));
        }
        Instr::LdGlobal { dst, ty, addr } => {
            out.push_str(&format!("    ld.global.{} {dst}, [{addr}];\n", ty.ptx()));
        }
        Instr::StGlobal { ty, addr, value } => {
            out.push_str(&format!(
                "    st.global.{} [{addr}], {};\n",
                ty.ptx(),
                value.ptx()
            ));
        }
        Instr::Setp { cmp, ty, dst, a, b } => {
            out.push_str(&format!(
                "    setp.{}.{} {dst}, {}, {};\n",
                cmp.ptx(),
                ty.ptx(),
                a.ptx(),
                b.ptx()
            ));
        }
        Instr::FArith { op, ty, dst, a, b } => {
            out.push_str(&format!(
                "    {}.rn.{} {dst}, {}, {};\n",
                op.ptx(),
                ty.ptx(),
                a.ptx(),
                b.ptx()
            ));
        }
        Instr::IDivRem { rem, ty, dst, a, b } => {
            out.push_str(&format!(
                "    {}.{} {dst}, {}, {};\n",
                if *rem { "rem" } else { "div" },
                ty.ptx(),
                a.ptx(),
                b.ptx()
            ));
        }
        Instr::Unary { op, ty, dst, src } => {
            let m = match op {
                UnOp::Neg => "neg",
                UnOp::Abs => "abs",
                UnOp::Sqrt => "sqrt.rn",
                UnOp::Ex2Approx => "ex2.approx",
            };
            out.push_str(&format!("    {m}.{} {dst}, {};\n", ty.ptx(), src.ptx()));
        }
        Instr::Fma { ty, dst, a, b, c } => {
            out.push_str(&format!(
                "    fma.rn.{} {dst}, {}, {}, {};\n",
                ty.ptx(),
                a.ptx(),
                b.ptx(),
                c.ptx()
            ));
        }
        Instr::Cvt {
            mode,
            dst_ty,
            src_ty,
            dst,
            src,
        } => {
            out.push_str(&format!(
                "    cvt{}.{}.{} {dst}, {src};\n",
                mode.ptx(),
                dst_ty.ptx(),
                src_ty.ptx()
            ));
        }
        Instr::Selp {
            ty,
            dst,
            a,
            b,
            pred,
        } => {
            out.push_str(&format!(
                "    selp.{} {dst}, {}, {}, {pred};\n",
                ty.ptx(),
                a.ptx(),
                b.ptx()
            ));
        }
        Instr::PredBin { op, dst, a, b } => {
            let m = match op {
                PredOp::And => "and",
                PredOp::Or => "or",
            };
            out.push_str(&format!("    {m}.pred {dst}, {a}, {b};\n"));
        }
        Instr::RedAdd { ty, addr, value } => {
            out.push_str(&format!(
                "    red.global.add.{} [{addr}], {value};\n",
                ty.ptx()
            ));
        }
        Instr::Guarded { pred, inner } => {
            let mut text = String::new();
            render_ptx_instr(inner, &mut text);
            out.push_str("    @");
            if pred.negated {
                out.push('!');
            }
            out.push_str(&pred.reg);
            out.push(' ');
            out.push_str(text.strip_prefix("    ").unwrap_or(&text));
        }
        Instr::WarpFold { ptx, .. } => out.push_str(ptx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    /// The `fmt::Write` impl is what lets 84 unconverted `writeln!`
    /// sites keep working, so the line splitting it does has to be
    /// exactly right — an off-by-one newline would move every golden.
    #[test]
    fn writeln_produces_one_instruction_per_line_including_its_newline() {
        let mut b = Body::new();
        writeln!(b, "    add.s32 %r1, %r2, %r3;").unwrap();
        writeln!(b, "    bar.sync 0;").unwrap();
        writeln!(b, "    ret;").unwrap();
        assert_eq!(b.len(), 3);
        // A line the grammar covers is lifted; one it does not stays text.
        assert!(matches!(
            b.code()[0],
            Instr::Bin {
                op: BinOp::Add,
                ty: Ty::S32,
                ..
            }
        ));
        assert_eq!(b.code()[1], Instr::Raw("    bar.sync 0;\n".to_string()));
        assert_eq!(
            b.render_ptx(),
            "    add.s32 %r1, %r2, %r3;\n    bar.sync 0;\n    ret;\n"
        );
    }

    /// `write_fmt` may deliver one line across several `write_str`
    /// calls, so a partial write must not become its own instruction.
    #[test]
    fn a_line_split_across_writes_is_still_one_instruction() {
        let mut b = Body::new();
        b.write_str("    add").unwrap();
        b.write_str(".s32 %r1, %r2, %r3;").unwrap();
        // `code()` rather than `len()`: `len()` is the splice-point
        // accessor and deliberately asserts it is not called mid-line,
        // which is exactly the state under test here.
        assert_eq!(
            b.code().len(),
            0,
            "nothing is an instruction until its newline"
        );
        b.write_str("\n").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.render_ptx(), "    add.s32 %r1, %r2, %r3;\n");
    }

    /// A multi-line `writeln!` (the emitter has several) splits into
    /// one instruction per line.
    #[test]
    fn a_multi_line_write_splits_per_line() {
        let mut b = Body::new();
        write!(b, "    a;\n    b;\n").unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b.render_ptx(), "    a;\n    b;\n");
    }

    /// Every structured variant renders the text its `Raw` predecessor
    /// did. These four spellings are load-bearing: they are what the
    /// goldens recorded before the variants existed.
    #[test]
    fn structured_variants_render_the_text_they_replaced() {
        let mut b = Body::new();
        b.push(Instr::ReadSpecial {
            dst: "%r3".into(),
            which: SpecialReg::ThreadIdxX,
        });
        b.push(Instr::ReadSpecial {
            dst: "%r4".into(),
            which: SpecialReg::BlockDimX,
        });
        b.push(Instr::ReadSpecial {
            dst: "%r5".into(),
            which: SpecialReg::BlockIdxX,
        });
        b.push(Instr::Label("L_done".into()));
        b.push(Instr::Bra {
            target: "L_done".into(),
            pred: None,
        });
        b.push(Instr::Bra {
            target: "L_done".into(),
            pred: Some(PredUse {
                reg: "%p1".into(),
                negated: false,
            }),
        });
        b.push(Instr::Bra {
            target: "L_done".into(),
            pred: Some(PredUse {
                reg: "%p1".into(),
                negated: true,
            }),
        });
        b.push(Instr::Ret);
        let want = concat!(
            "    mov.u32 %r3, %tid.x;
",
            "    mov.u32 %r4, %ntid.x;
",
            "    mov.u32 %r5, %ctaid.x;
",
            "L_done:
",
            "    bra L_done;
",
            "    @%p1 bra L_done;
",
            "    @!%p1 bra L_done;
",
            "    ret;
",
        );
        assert_eq!(b.render_ptx(), want);
    }

    /// `has_branch_since` replaces a `contains("bra ")` over a byte
    /// range, and has to see a branch in either representation — during
    /// the migration a body holds both.
    #[test]
    fn a_branch_is_seen_whether_it_is_structured_or_raw() {
        let mut b = Body::new();
        writeln!(b, "    add.s32 %r1, %r2, %r3;").unwrap();
        let mark = b.len();
        assert!(!b.has_branch_since(mark));
        b.push(Instr::Bra {
            target: "L".into(),
            pred: None,
        });
        assert!(b.has_branch_since(mark));

        let mut r = Body::new();
        writeln!(r, "    add.s32 %r1, %r2, %r3;").unwrap();
        let mark = r.len();
        writeln!(r, "    bra L_done;").unwrap();
        assert!(r.has_branch_since(mark));
    }

    /// The splice is what the bounds prologue needs, and it has to land
    /// the instructions in order at the recorded point.
    #[test]
    fn a_splice_lands_in_order_at_the_recorded_index() {
        let mut b = Body::new();
        writeln!(b, "    guard;").unwrap();
        let at = b.len();
        writeln!(b, "    body;").unwrap();
        let mut pro = Body::new();
        writeln!(pro, "    pre1;").unwrap();
        writeln!(pro, "    pre2;").unwrap();
        b.splice_at(at, pro);
        assert_eq!(
            b.render_ptx(),
            "    guard;\n    pre1;\n    pre2;\n    body;\n"
        );
    }
}
