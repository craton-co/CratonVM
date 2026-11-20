// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! An explicit `new StringBuilder().append(..)...toString()` chain fused into
//! one concatenation: JIT round 21 wave 20, lane sbfuse, STRINTR21-4
//! (`docs/feature-designs/jit-ir-optimizations.md`;
//! `CRATONVM_JIT_IR_SB_CHAIN_FUSE`, default ON, `=0` builds the chain as
//! before).
//!
//! # What it does
//!
//! HotSpot's C2 (`OptimizeStringConcat`) removes a builder that is created,
//! appended to and `toString()`'d in one straight-line region: one exact-size
//! `byte[]` and one `String`. Both CratonVM tiers built it -- a real
//! `<init>` call chain, a 16-byte buffer that grows, a copy in `toString()`
//! (`R21StrintrRows sb-chunk` 552 ns against HotSpot's 17).
//!
//! An explicit chain is the indy `makeConcatWithConstants` recipe spelled as
//! bytecode. [`recognize_sb_chain`] reads it back as that recipe
//! ([`SbChain::recipe`]): a constant operand (`ldc` of a `String` literal, an
//! `int` constant appended as `char` or `int`, `lconst` appended as `long`)
//! becomes literal text, every other operand an argument. The VM makes a
//! process-lived concat bridge site of the recipe
//! (`CompileRequest::sb_concat_site_resolver`), and the optimizing tier's
//! builder replaces the whole chain by ONE `Op::IndyBridge` of that site
//! (`IrBuilder::sb_chain_fuse_step`), whose lowering is the existing inline
//! concat (`ir_lower::lower_indy_bridge_concat_inline`, STRGAP4-1) with the
//! bridge call -- the interpreter's own concat renderer -- as its decline
//! edge. Nothing is committed before the result exists, so every decline
//! (a UTF16 operand, a full TLAB, compact strings off) is that call over the
//! same operands.
//!
//! # Why the result is the JDK's, byte for byte
//!
//! * `append(String)` renders `"null"` for `null` and the String's own code
//!   units otherwise; `append(char)` one code unit; `append(int)` /
//!   `append(long)` `Integer` / `Long.toString`. The concat renderer and the
//!   inline plan render exactly those (`ConcatArgKind::String` / `Char` /
//!   `Int` / `Long`).
//! * The coder: `StringBuilder.toString()` compresses a UTF16 buffer back to
//!   LATIN1 when every unit fits (`StringUTF16.newString` under compact
//!   strings), so its result is LATIN1 exactly when every unit is `<= 0xFF`
//!   -- the concat's rule. A wide literal makes the whole result UTF16 in
//!   both.
//! * Identity: a non-empty result is a fresh `String` in both. An EMPTY
//!   result is not: the builder answers the `""` literal
//!   (`StringLatin1.newString` with `len == 0`), the concat renderer a fresh
//!   empty String. So a chain whose result can be empty -- every operand a
//!   `String`, no non-empty literal, or no append at all -- was refused. It
//!   is fused since CONF21-2 (`CRATONVM_JIT_IR_SB_EMPTY_RESULT_FRESH`,
//!   default ON; `=0` refuses it again): C2's `OptimizeStringConcat` answers
//!   every fused chain with a fresh String, an empty one included, and never
//!   hands back an operand. Measured on HotSpot 25 (`ConfSbSteady`, 600
//!   batches of 1000 calls): `new StringBuilder().toString() == ""`,
//!   `new StringBuilder().append("").toString() == ""` and, for `s == ""`,
//!   `new StringBuilder().append(s).toString() == s` all answer false in C2's
//!   steady state and true under `-Xint`, `-XX:-OptimizeStringConcat` and
//!   `-XX:TieredStopAtLevel=1`; for a non-empty `s` the last is false
//!   everywhere. So the optimizing tier answers what C2 does, and the
//!   single-pass tier keeps the literal, as C1 does.
//! * Exceptions: the only throwable left is the `OutOfMemoryError` of the
//!   result, raised by the bridge at the `new`'s bci, whose handler set is
//!   the chain's (a chain that crosses a `try` boundary is refused).
//!
//! # What it admits ([`recognize_sb_chain`])
//!
//! `new java/lang/StringBuilder; dup; invokespecial <init>()V` (or
//! `<init>(String)V` when its argument is a non-null String literal, or
//! `<init>(I)V` when its argument is a constant in `0..=32767`, SBFUSE21-3),
//! then zero or more `invokevirtual append(X)` with `X` one of `String`,
//! `char`, `int`, `long`, `boolean` and (SBFUSE21-5, while
//! `CRATONVM_JIT_CONCAT_FP_INLINE` is on) `float` / `double`, each preceded by
//! operand code that pushes exactly that one value from locals, constants and
//! side-effect-free arithmetic. Instance-field
//! reads and array loads are admitted when their trap nodes can re-enter at
//! the `new` snapshot; calls, static-field reads, division, stores and branches
//! remain refused. Then `invokevirtual
//! toString()`. The builder may live on the operand stack or in a local for a
//! straight-line, non-escaping chain; local receiver loads and discarded
//! append results are removed as part of fusion. Refused: a builder used after
//! `toString()` (including a loop-carried builder, and a local builder whose
//! slot is read before the chain inside a loop: the fusion drops the `astore`),
//! a builder referenced a second time while an append result is still on the
//! stack, `append(Object)` /
//! `append(CharSequence)` / `append(char[])` / ..., any other builder member
//! (`setLength`, `insert`, `length`, `charAt`, ...), a branch target or an
//! exception-handler boundary inside the chain, and a recipe the inline plan
//! does not admit (`JitConcatInlinePlan::from_recipe`).

use crate::concat_inline_plan::{JitConcatInlinePlan, CONCAT_TAG_ARG, CONCAT_TAG_CONST};

/// `CRATONVM_JIT_IR_SB_EMPTY_RESULT_FRESH` (CONF21-2, page
/// `jr21-conf-string-identity-differs-only-from-hotspot-c2-FIXED-20261027.md`; HotSpot
/// conformance, default ON, `=0` refuses a chain whose result may be empty, as
/// before, so it answers the `""` literal in every tier). Read per compile.
pub fn sb_empty_result_fresh_enabled() -> bool {
    true
}

/// The builder class the fusion serves.
pub const SB_CLASS: &str = "java/lang/StringBuilder";

/// Most instructions one operand may take (a bound on what the scan reads,
/// not a semantic limit).
pub const SB_CHAIN_MAX_OPERAND_INSNS: usize = 12;

/// Most bytes from the `new` to the `toString()` (a bound on the scan).
pub const SB_CHAIN_MAX_BYTES: usize = 1024;

/// Largest constant capacity `new StringBuilder(int)` may carry and still be
/// fused (SBFUSE21-3, `sbf21`): the `sipush` range, so the elided
/// `byte[capacity]` is at most 32 KiB and its allocation cannot be the
/// observable outcome of the constructor.
pub const SB_CHAIN_MAX_CAPACITY: i64 = i16::MAX as i64;

/// `CRATONVM_JIT_IR_SB_CHAIN_FUSE` (JIT round 21 wave 20, lane sbfuse,
/// STRINTR21-4; perf, default ON, `=0` builds every chain as before and puts
/// its builder sites back under the String-intrinsic pin). Read per compile,
/// no process global.
pub fn sb_chain_fuse_enabled() -> bool {
    true
}

/// The value one `append` renders, from its descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SbAppendKind {
    /// `append(Ljava/lang/String;)`: the String's units, or `"null"`.
    String,
    /// `append(C)`: one code unit.
    Char,
    /// `append(I)`: `Integer.toString`.
    Int,
    /// `append(J)`: `Long.toString`.
    Long,
    /// `append(Z)`: the lowercase Boolean text.
    Boolean,
    /// `append(F)`: `Float.toString` (SBFUSE21-5, `sbf21`; admitted only
    /// while `CRATONVM_JIT_CONCAT_FP_INLINE` is on, see [`Self::needs_fp`]).
    Float,
    /// `append(D)`: `Double.toString` (same gate as [`Self::Float`]).
    Double,
}

impl SbAppendKind {
    /// The kind of an `append` with this descriptor, or `None` for every
    /// overload the fusion does not render (`Object`, `CharSequence`,
    /// `char[]`, `StringBuffer`, ...).
    pub fn from_append_descriptor(descriptor: &str) -> Option<Self> {
        match descriptor {
            "(Ljava/lang/String;)Ljava/lang/StringBuilder;" => Some(Self::String),
            "(C)Ljava/lang/StringBuilder;" => Some(Self::Char),
            "(I)Ljava/lang/StringBuilder;" => Some(Self::Int),
            "(J)Ljava/lang/StringBuilder;" => Some(Self::Long),
            "(Z)Ljava/lang/StringBuilder;" => Some(Self::Boolean),
            "(F)Ljava/lang/StringBuilder;" => Some(Self::Float),
            "(D)Ljava/lang/StringBuilder;" => Some(Self::Double),
            _ => None,
        }
    }

    /// The parameter type of this kind in the fused site's descriptor.
    pub fn param(self) -> &'static str {
        match self {
            Self::String => "Ljava/lang/String;",
            Self::Char => "C",
            Self::Int => "I",
            Self::Long => "J",
            Self::Boolean => "Z",
            Self::Float => "F",
            Self::Double => "D",
        }
    }

    /// Does rendering this kind use the concat FP formatter? Such a chain is
    /// fused only while `CRATONVM_JIT_CONCAT_FP_INLINE` (the FP concat kill
    /// switch, default ON) is on; `=0` leaves the builder calls as before.
    /// Tag stored in `VirtualObjectState::field_values` for [`VIRTUAL_STRING_BUILDER_ATYPE`].
    pub const fn virtual_tag(self) -> i64 {
        match self {
            Self::String => crate::deopt::SB_VIRTUAL_TAG_STRING,
            Self::Char => crate::deopt::SB_VIRTUAL_TAG_CHAR,
            Self::Int => crate::deopt::SB_VIRTUAL_TAG_INT,
            Self::Long => crate::deopt::SB_VIRTUAL_TAG_LONG,
            Self::Boolean => crate::deopt::SB_VIRTUAL_TAG_BOOLEAN,
            Self::Float => crate::deopt::SB_VIRTUAL_TAG_FLOAT,
            Self::Double => crate::deopt::SB_VIRTUAL_TAG_DOUBLE,
        }
    }

    pub fn needs_fp(self) -> bool {
        matches!(self, Self::Float | Self::Double)
    }
}

/// One `append` of a chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SbChainPart {
    /// A constant operand folded into literal text: the single instruction at
    /// `operand_pc` (an `ldc` String literal, an `int` constant appended as
    /// `char` / `int`, `lconst` appended as `long`). The builder skips from
    /// `operand_pc` past the `append` at `append_pc`.
    Literal {
        text: Vec<u16>,
        operand_pc: usize,
        append_pc: usize,
    },
    /// A run-time operand: the code from `operand_pc` up to `append_pc`
    /// pushes exactly one value of `kind`, which the builder builds with its
    /// ordinary arms and takes off the stack at `append_pc`.
    Value {
        kind: SbAppendKind,
        operand_pc: usize,
        append_pc: usize,
    },
}

impl SbChainPart {
    /// The first pc of this part's operand code.
    pub fn operand_pc(&self) -> usize {
        match self {
            Self::Literal { operand_pc, .. } | Self::Value { operand_pc, .. } => *operand_pc,
        }
    }

    /// The pc of this part's `invokevirtual append`.
    pub fn append_pc(&self) -> usize {
        match self {
            Self::Literal { append_pc, .. } | Self::Value { append_pc, .. } => *append_pc,
        }
    }
}

/// A recognized chain. See the module doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbChain {
    /// The `new java/lang/StringBuilder`.
    pub new_pc: usize,
    /// The chain's `<init>` invoke bytecode index (constructors with a literal
    /// String argument are one instruction later than `()V`).
    pub init_bci: usize,
    /// First bytecode after the constructor. The optimizing builder jumps here
    /// when it replaces allocation and construction with the concat plan.
    pub first_operand_pc: usize,
    /// A non-null String literal passed to `StringBuilder(String)`, folded as
    /// the first concat segment.
    pub constructor_literal: Option<Vec<u16>>,
    /// The `invokevirtual toString()`.
    pub to_string_pc: usize,
    /// Every `append`, in order.
    pub parts: Vec<SbChainPart>,
    /// True if any operand contains a method call (`invokevirtual`/`invokestatic`/`invokeinterface`).
    pub has_call: bool,
    /// The receiver loads and initial store of a non-escaping local builder.
    pub local: Option<SbChainLocal>,
}

/// Bytecode positions to skip for a builder held in a local during its chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbChainLocal {
    /// Local variable slot holding the builder.
    pub slot: u16,
    /// The first instruction after the constructor's `astore`.
    pub first_operand_pc: usize,
    /// Receiver load bytecode ranges, one for each append then `toString`.
    /// One entry for each append and `toString`; `None` means the previous
    /// append result already supplies the receiver on the operand stack.
    pub receiver_loads: Vec<Option<(usize, usize)>>,
    /// Whether each append's result is discarded by a following `pop`.
    pub append_result_pops: Vec<bool>,
    /// Pure stack-building bytecode between the final append and the local
    /// receiver load for `toString`, if any.
    pub to_string_prefix: Option<(usize, usize)>,
}

/// The concat recipe of a chain, in `StringConcatFactory` terms: tags only
/// (`\1` an argument, `\2` a constant), every literal text a constant (so a
/// literal unit `1` or `2` can never read as a tag), and the descriptor
/// `(<one parameter per Value part>)Ljava/lang/String;`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbChainRecipe {
    pub recipe: Vec<u16>,
    pub constants: Vec<Vec<u16>>,
    pub descriptor: String,
}

impl SbChainRecipe {
    /// The inline plan of this recipe, or `None` (a chain the plan does not
    /// admit is not fused).
    pub fn plan(&self) -> Option<JitConcatInlinePlan> {
        let constants: Vec<&[u16]> = self.constants.iter().map(Vec::as_slice).collect();
        JitConcatInlinePlan::from_recipe(&self.recipe, &constants, &self.descriptor)
    }
}

impl SbChain {
    /// The `invokespecial <init>`.
    pub fn init_pc(&self) -> usize {
        self.init_bci
    }

    /// The first pc after `new; dup; [literal]; invokespecial <init>`.
    pub fn first_operand_pc(&self) -> usize {
        self.local
            .as_ref()
            .map_or(self.first_operand_pc, |local| local.first_operand_pc)
    }

    /// The pc after the `toString()`.
    pub fn end_pc(&self) -> usize {
        self.to_string_pc + 3
    }

    /// Is `pc` strictly inside the chain (past its `new`, before its end)?
    pub fn contains_pc(&self, pc: usize) -> bool {
        pc > self.new_pc && pc < self.end_pc()
    }

    /// The pcs of the chain's builder invokes: `<init>`, every `append`,
    /// `toString()`.
    pub fn invoke_pcs(&self) -> Vec<usize> {
        let mut pcs = Vec::with_capacity(self.parts.len() + 2);
        pcs.push(self.init_pc());
        pcs.extend(self.parts.iter().map(SbChainPart::append_pc));
        pcs.push(self.to_string_pc);
        pcs
    }

    /// How many run-time operands ([`SbChainPart::Value`]) the chain has.
    pub fn value_count(&self) -> usize {
        self.parts
            .iter()
            .filter(|p| matches!(p, SbChainPart::Value { .. }))
            .count()
    }

    /// The chain's concat recipe. See [`SbChainRecipe`].
    pub fn recipe(&self) -> SbChainRecipe {
        let mut recipe =
            Vec::with_capacity(self.parts.len() + usize::from(self.constructor_literal.is_some()));
        let mut constants = Vec::new();
        let mut descriptor = String::from("(");
        if let Some(text) = &self.constructor_literal {
            recipe.push(CONCAT_TAG_CONST);
            constants.push(text.clone());
        }
        for part in &self.parts {
            match part {
                SbChainPart::Literal { text, .. } => {
                    recipe.push(CONCAT_TAG_CONST);
                    constants.push(text.clone());
                }
                SbChainPart::Value { kind, .. } => {
                    recipe.push(CONCAT_TAG_ARG);
                    descriptor.push_str(kind.param());
                }
            }
        }
        descriptor.push_str(")Ljava/lang/String;");
        SbChainRecipe {
            recipe,
            constants,
            descriptor,
        }
    }

    /// Can the result be empty? Then the builder answers the `""` literal and
    /// the concat (and C2) a fresh String (module doc, "Identity"). True for a
    /// chain with no append.
    fn may_be_empty(&self) -> bool {
        if self
            .constructor_literal
            .as_ref()
            .is_some_and(|text| !text.is_empty())
        {
            return false;
        }
        !self.parts.iter().any(|p| match p {
            SbChainPart::Literal { text, .. } => !text.is_empty(),
            SbChainPart::Value { kind, .. } => *kind != SbAppendKind::String,
        })
    }
}

/// The method facts the recognizer reads.
pub struct SbChainCode<'a> {
    /// The method's bytecode, `code_len` bytes.
    pub code: &'a [u8],
    /// `branch_targets[pc]`: some instruction names `pc` as an explicit
    /// target (`bytecode_analysis::branch_target_map`).
    pub branch_targets: &'a [bool],
    /// The exception table, `(start_pc, end_pc, handler_pc)`.
    pub handlers: &'a [(usize, usize, usize)],
}

/// `(values popped, values pushed, instruction length)` of an instruction an
/// operand may use: loads, constants, and arithmetic that cannot throw or
/// trap. `None` for everything else (`ldc` is handled by the caller).
fn parse_method_descriptor_counts(descriptor: &str) -> Option<(usize, &str)> {
    let inner = descriptor.strip_prefix('(')?;
    let close = inner.find(')')?;
    let params_str = &inner[..close];
    let ret = &inner[close + 1..];
    let bytes = params_str.as_bytes();
    let mut i = 0usize;
    let mut count = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'L' {
            let semi = bytes[i + 1..].iter().position(|&b| b == b';')?;
            i += 1 + semi + 1;
        } else if matches!(bytes[i], b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z') {
            i += 1;
        } else {
            return None;
        }
        count += 1;
    }
    Some((count, ret))
}

fn is_admitted_operand_return(ret: &str) -> bool {
    matches!(
        ret,
        "Ljava/lang/String;" | "Z" | "C" | "B" | "S" | "I" | "J" | "F" | "D"
    )
}

fn call_operand_effect(
    op: u8,
    pc: usize,
    code: &[u8],
    invoke_at: &dyn Fn(usize) -> Option<(String, String, String)>,
) -> Result<(usize, usize, usize), &'static str> {
    let len = match op {
        0xb6 | 0xb8 => {
            if pc + 3 > code.len() {
                return Err("truncated call in operand");
            }
            3
        }
        0xb9 => {
            if pc + 5 > code.len() {
                return Err("truncated invokeinterface in operand");
            }
            5
        }
        _ => return Err("operand code outside the pure set (a call, a field, a store, ...)"),
    };
    let (_class, name, descriptor) = invoke_at(pc).ok_or("unresolved call in operand")?;
    if name == "<init>" || name == "<clinit>" {
        return Err("constructor or class-initializer call in operand");
    }
    let (params, ret) =
        parse_method_descriptor_counts(&descriptor).ok_or("malformed call descriptor in operand")?;
    if !is_admitted_operand_return(ret) {
        return Err("operand call return type is not a String or primitive");
    }
    let receiver = usize::from(op != 0xb8);
    Ok((params + receiver, 1, len))
}

fn pure_operand_effect(
    code: &[u8],
    pc: usize,
    field_at: &dyn Fn(u16) -> Option<u8>,
) -> Option<(usize, usize, usize)> {
    Some(match *code.get(pc)? {
        // iconst_m1..iconst_5, lconst_0, lconst_1
        0x02..=0x0a => (0, 1, 1),
        0x10 => (0, 1, 2), // bipush
        0x11 => (0, 1, 3), // sipush
        // iload, lload, aload
        0x15 | 0x16 | 0x19 => (0, 1, 2),
        // iload_n, lload_n, aload_n
        0x1a..=0x21 | 0x2a..=0x2d => (0, 1, 1),
        // iadd ladd isub lsub imul lmul, shifts, and/or/xor (int and long)
        0x60 | 0x61 | 0x64 | 0x65 | 0x68 | 0x69 | 0x78..=0x83 => (2, 1, 1),
        // ineg lneg i2l l2i i2b i2c i2s
        0x74 | 0x75 | 0x85 | 0x88 | 0x91..=0x93 => (1, 1, 1),
        // SBFUSE21-5 (`sbf21`): the float / double operands of
        // `append(F)` / `append(D)`. fconst_0..2, dconst_0..1
        0x0b..=0x0f => (0, 1, 1),
        // fload, dload
        0x17 | 0x18 => (0, 1, 2),
        // fload_n, dload_n
        0x22..=0x29 => (0, 1, 1),
        // fadd dadd fsub dsub fmul dmul (none can throw; division and
        // remainder stay refused)
        0x62 | 0x63 | 0x66 | 0x67 | 0x6a | 0x6b => (2, 1, 1),
        // fneg dneg i2f i2d l2f l2d f2d d2f
        0x76 | 0x77 | 0x86 | 0x87 | 0x89 | 0x8a | 0x8d | 0x90 => (1, 1, 1),
        // getfield is a read with a possible null trap. Its guard is re-stamped
        // to the chain's `new` snapshot by the IR builder, so a trap replays
        // the original allocation and operand sequence in the interpreter.
        0xb4 => {
            let cp = u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
            field_at(cp)?;
            (1, 1, 3)
        }
        // Array element reads and arraylength can trap on null or bounds; they
        // use the same re-execute-at-new frame treatment as getfield.
        0x2e..=0x35 => (2, 1, 1),
        0xbe => (1, 1, 1),
        _ => return None,
    })
}

/// The local slot and instruction length for an `aload` / `astore` of a
/// StringBuilder receiver.
fn builder_local_access(code: &[u8], pc: usize, load: bool) -> Option<(u16, usize)> {
    let op = *code.get(pc)?;
    let (slot, len) = match (load, op) {
        (true, 0x2a..=0x2d) => ((op - 0x2a) as u16, 1),
        (false, 0x4b..=0x4e) => ((op - 0x4b) as u16, 1),
        (true, 0x19) | (false, 0x3a) => (*code.get(pc + 1)? as u16, 2),
        (true, 0xc4) if code.get(pc + 1) == Some(&0x19) => (
            u16::from_be_bytes([*code.get(pc + 2)?, *code.get(pc + 3)?]),
            4,
        ),
        (false, 0xc4) if code.get(pc + 1) == Some(&0x3a) => (
            u16::from_be_bytes([*code.get(pc + 2)?, *code.get(pc + 3)?]),
            4,
        ),
        _ => return None,
    };
    Some((slot, len))
}

/// The value of a one-instruction integral constant at `pc`, with its length.
fn integral_constant(code: &[u8], pc: usize) -> Option<(i64, usize)> {
    let op = *code.get(pc)?;
    Some(match op {
        0x02..=0x08 => (i64::from(op) - 3, 1),
        0x09 => (0, 1),
        0x0a => (1, 1),
        // Cast: bipush's operand byte is signed.
        0x10 => (i64::from(*code.get(pc + 1)? as i8), 2),
        0x11 => (
            i64::from(i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?])),
            3,
        ),
        _ => return None,
    })
}

/// The value and length of a one-instruction `int` constant (`iconst_*`,
/// `bipush`, `sipush`) at `pc`: [`integral_constant`] without the `long`
/// constants, which no `int` parameter can take.
fn capacity_constant(code: &[u8], pc: usize) -> Option<(i64, usize)> {
    if code.get(pc) == Some(&0x09) || code.get(pc) == Some(&0x0a) {
        return None;
    }
    integral_constant(code, pc)
}

/// Is `op` an `ldc` / `ldc_w`, and how long is it?
fn ldc_len(op: u8) -> Option<usize> {
    match op {
        0x12 => Some(2),
        0x13 => Some(3),
        _ => None,
    }
}

/// The literal text a one-instruction constant operand of `kind` renders, or
/// `None` when the operand is not such a constant.
fn literal_operand(
    code: &[u8],
    operand_pc: usize,
    append_pc: usize,
    kind: SbAppendKind,
    ldc_string_at: &dyn Fn(usize) -> Option<Vec<u16>>,
) -> Option<Vec<u16>> {
    let op = *code.get(operand_pc)?;
    if let Some(len) = ldc_len(op) {
        if kind == SbAppendKind::String && operand_pc + len == append_pc {
            return ldc_string_at(operand_pc);
        }
        return None;
    }
    let (value, len) = integral_constant(code, operand_pc)?;
    if operand_pc + len != append_pc {
        return None;
    }
    let is_long_const = matches!(op, 0x09 | 0x0a);
    match kind {
        // Cast: `append(char)` of an int constant is its low 16 bits (the
        // verifier's int-to-char narrowing at the call).
        SbAppendKind::Char if !is_long_const => Some(vec![value as u16]),
        SbAppendKind::Int if !is_long_const => Some(value.to_string().encode_utf16().collect()),
        SbAppendKind::Long if is_long_const => Some(value.to_string().encode_utf16().collect()),
        _ => None,
    }
}

/// sbf21: does some read of local `slot` BEFORE the chain (`new_pc`) run again
/// after it? The fusion drops the chain's `astore`, so such a read would see
/// the slot's previous value instead of the builder
/// (`StringBuilder sb = null; for (..) { if (sb != null) use(sb); sb = new
/// StringBuilder(); sb.append(i); s = sb.toString(); }`). A read at a lower pc
/// runs after the chain only through a backward branch from beyond the chain
/// to the read or before it, or through a handler of a range covering the
/// chain that sits at or before the read; both are checked conservatively
/// (a slot merely REUSED for an unrelated variable inside the same loop is
/// refused too, which costs coverage, never correctness).
fn local_slot_read_reaches_chain(
    src: &SbChainCode<'_>,
    slot: u16,
    new_pc: usize,
    end_pc: usize,
) -> bool {
    let code = src.code;
    let mut reads = Vec::new();
    let mut pc = 0usize;
    while pc < new_pc.min(code.len()) {
        if builder_local_access(code, pc, true).is_some_and(|(loaded, _)| loaded == slot) {
            reads.push(pc);
        }
        pc += crate::bytecode_analysis::step(code, pc).max(1);
    }
    let Some(&last_read) = reads.iter().max() else {
        return false;
    };
    let back_edge = crate::bytecode_analysis::back_edges(code, code.len())
        .into_iter()
        .any(|(target, source)| source >= end_pc && target <= last_read);
    let handler = src
        .handlers
        .iter()
        .any(|&(start, stop, handler)| start <= new_pc && stop >= end_pc && handler <= last_read);
    back_edge || handler
}

/// Recognize the chain whose `new` is at `new_pc`, or name why not. Pure.
///
/// `invoke_at(pc)` answers the `(class, name, descriptor)` of the invoke
/// instruction at `pc` (from its constant-pool reference), `ldc_string_at(pc)`
/// the UTF-16 text of the `ldc` / `ldc_w` at `pc` when it loads a `String`
/// literal (and `None` for any other constant).
pub fn recognize_sb_chain(
    src: &SbChainCode<'_>,
    new_pc: usize,
    invoke_at: &dyn Fn(usize) -> Option<(String, String, String)>,
    ldc_string_at: &dyn Fn(usize) -> Option<Vec<u16>>,
    field_at: &dyn Fn(u16) -> Option<u8>,
) -> Result<SbChain, &'static str> {
    let code = src.code;
    if code.get(new_pc) != Some(&0xbb) {
        return Err("not a new");
    }
    if code.get(new_pc + 3) != Some(&0x59) {
        return Err("the new is not followed by dup");
    }
    let constructor_arg_pc = new_pc + 4;
    // SBFUSE21-3 (`sbf21`): `new StringBuilder(<non-negative int constant>)`.
    let mut capacity_constructor = false;
    let mut capacity_value = 0i64;
    let (init_pc, constructor_literal) = if code.get(constructor_arg_pc) == Some(&0xb7) {
        (constructor_arg_pc, None)
    } else if let Some(len) = code.get(constructor_arg_pc).and_then(|op| ldc_len(*op)) {
        let init_pc = constructor_arg_pc + len;
        if code.get(init_pc) != Some(&0xb7) {
            return Err("the new is not followed by dup; invokespecial");
        }
        let literal = ldc_string_at(constructor_arg_pc)
            .ok_or("StringBuilder(String) constructor argument is not a String literal")?;
        (init_pc, Some(literal))
    } else if let Some((capacity, len)) = capacity_constant(code, constructor_arg_pc) {
        if code.get(constructor_arg_pc + len) != Some(&0xb7) {
            return Err("the new is not followed by dup; invokespecial");
        }
        capacity_constructor = true;
        capacity_value = capacity;
        (constructor_arg_pc + len, None)
    } else {
        return Err("the new is not followed by dup; invokespecial");
    };
    let (class, name, descriptor) = invoke_at(init_pc).ok_or("unresolved <init>")?;
    if class != SB_CLASS || name != "<init>" {
        return Err("not a java/lang/StringBuilder");
    }
    if descriptor != "()V"
        && !(descriptor == "(Ljava/lang/String;)V" && constructor_literal.is_some())
        && !(descriptor == "(I)V" && capacity_constructor)
    {
        return Err("a StringBuilder constructor with arguments");
    }
    if capacity_constructor && descriptor != "(I)V" {
        return Err("a StringBuilder capacity constant before a different constructor");
    }
    // The capacity is unobservable once the builder is gone; a negative one
    // must keep throwing `NegativeArraySizeException`, so it is refused (a
    // variable capacity never gets here: it is no constant).
    if capacity_constructor && !(0..=SB_CHAIN_MAX_CAPACITY).contains(&capacity_value) {
        return Err("a StringBuilder capacity constructor with a refused capacity");
    }
    let first_operand_pc = init_pc + 3;
    let is_target = |pc: usize| src.branch_targets.get(pc).copied().unwrap_or(false);
    if is_target(new_pc + 3) || is_target(constructor_arg_pc) || is_target(init_pc) {
        return Err("a branch target inside the chain");
    }
    let mut parts: Vec<SbChainPart> = Vec::new();
    let mut pc = first_operand_pc;
    let mut local = None;
    if let Some((slot, len)) = builder_local_access(code, pc, false) {
        if is_target(pc) {
            return Err("a branch target inside the chain");
        }
        local = Some((
            slot,
            SbChainLocal {
                slot,
                first_operand_pc: pc + len,
                receiver_loads: Vec::new(),
                append_result_pops: Vec::new(),
                to_string_prefix: None,
            },
        ));
        pc += len;
    }
    let mut operand_pc = pc;
    // Values the current operand's code has pushed and not consumed.
    let mut depth = 0usize;
    let mut insns = 0usize;
    let mut operand_has_ldc = false;
    let mut has_call = false;
    let mut stack_builder = false;
    let mut last_append_end = None;
    let mut append_receiver_load = None;
    let (to_string_pc, _to_string_receiver) = loop {
        if pc >= code.len() || pc - new_pc > SB_CHAIN_MAX_BYTES {
            return Err("the chain runs past the scan bound");
        }
        if is_target(pc) {
            return Err("a branch target inside the chain");
        }
        if let Some((slot, local)) = &mut local {
            if let Some((loaded, len)) = builder_local_access(code, pc, true) {
                if loaded == *slot && code.get(pc + len) == Some(&0xb6) {
                    // (sbf21: only an `invokevirtual` is a call; `invoke_at`
                    // reads a constant-pool index from the next two bytes of
                    // WHATEVER instruction follows.)
                    if let Some((class, name, descriptor)) = invoke_at(pc + len) {
                        if class == SB_CLASS
                            && name == "toString"
                            && descriptor == "()Ljava/lang/String;"
                        {
                            if stack_builder {
                                // sbf21: `foo(sb.append(1), sb.toString())`:
                                // an append result is still on the stack, so
                                // this `aload; toString` is a second use of
                                // the builder and the first escapes.
                                return Err(
                                    "the local builder is referenced while an append result is on the stack",
                                );
                            }
                            if is_target(pc) || is_target(pc + len) {
                                return Err("a branch target inside the chain");
                            }
                            let range = (pc, pc + len);
                            local.receiver_loads.push(Some(range));
                            let start = last_append_end.unwrap_or(local.first_operand_pc);
                            local.to_string_prefix = Some((start, pc));
                            break (pc + len, Some(range));
                        }
                    }
                }
            }
        }
        let receiver = if depth == 0 && insns == 0 {
            if let Some((slot, local)) = &mut local {
                if let Some((loaded, len)) = builder_local_access(code, pc, true) {
                    if loaded == *slot {
                        if stack_builder {
                            // sbf21: the previous append's result is still
                            // on the stack and the builder is loaded again
                            // (`foo(sb.append(1), sb.append(2))`).
                            return Err(
                                "the local builder is referenced while an append result is on the stack",
                            );
                        }
                        let receiver_pc = pc;
                        pc += len;
                        if is_target(receiver_pc) || is_target(pc) {
                            return Err("a branch target inside the chain");
                        }
                        operand_pc = pc;
                        let range = (receiver_pc, pc);
                        append_receiver_load = Some(range);
                        stack_builder = true;
                        Some(range)
                    } else {
                        if stack_builder {
                            append_receiver_load = None;
                        }
                        None
                    }
                } else if stack_builder {
                    append_receiver_load = None;
                    None
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let op = code[pc];
        if op == 0xb6 {
            let (class, name, descriptor) =
                invoke_at(pc).ok_or("unresolved invokevirtual in the chain")?;
            if class == SB_CLASS {
                if name == "toString" && descriptor == "()Ljava/lang/String;" && depth == 0 {
                    if let Some((_, local)) = &mut local {
                        if local.receiver_loads.len() != parts.len() + 1 {
                            if !stack_builder {
                                return Err("toString has no local or stack builder receiver");
                            }
                            local.receiver_loads.push(None);
                        }
                    }
                    break (pc, receiver);
                }
                if name != "append" {
                    return Err(
                        "a builder member other than append / toString (setLength, insert, ...)",
                    );
                }
                if local.is_some() && !stack_builder {
                    return Err("a local chain append has no builder receiver");
                }
                if let Some((_, local)) = &mut local {
                    local.receiver_loads.push(append_receiver_load.take());
                }
                let kind = SbAppendKind::from_append_descriptor(&descriptor).ok_or(
                    "an append overload the concat does not render (Object, CharSequence, ...)",
                )?;
                if kind.needs_fp() && !crate::concat_inline_plan::concat_fp_inline_enabled() {
                    return Err("a float / double append with CRATONVM_JIT_CONCAT_FP_INLINE=0");
                }
                if depth != 1 {
                    return Err("an append operand that is not one value of the chain's own code");
                }
                let part = match literal_operand(code, operand_pc, pc, kind, ldc_string_at) {
                    Some(text) => SbChainPart::Literal {
                        text,
                        operand_pc,
                        append_pc: pc,
                    },
                    None if operand_has_ldc => {
                        return Err("an ldc that is not a String literal operand")
                    }
                    None => SbChainPart::Value {
                        kind,
                        operand_pc,
                        append_pc: pc,
                    },
                };
                parts.push(part);
                pc += 3;
                if local.is_some() {
                    if code.get(pc) == Some(&0x57) {
                        if is_target(pc) {
                            return Err("a branch target inside the chain");
                        }
                        if let Some((_, local)) = &mut local {
                            local.append_result_pops.push(true);
                        }
                        pc += 1;
                        stack_builder = false;
                    } else {
                        if let Some((_, local)) = &mut local {
                            local.append_result_pops.push(false);
                        }
                        stack_builder = true;
                    }
                    last_append_end = Some(pc);
                }
                operand_pc = pc;
                depth = 0;
                insns = 0;
                operand_has_ldc = false;
                continue;
            }
        }
        let (pops, pushes, len) = if let Some(len) = ldc_len(op) {
            operand_has_ldc = true;
            (0, 1, len)
        } else if let Some(eff) = pure_operand_effect(code, pc, field_at) {
            eff
        } else {
            has_call = true;
            call_operand_effect(op, pc, code, invoke_at)?
        };
        if depth < pops {
            return Err("operand code that reads below its own values (the builder escapes)");
        }
        depth = depth - pops + pushes;
        insns += 1;
        if insns > SB_CHAIN_MAX_OPERAND_INSNS {
            return Err("an operand longer than the scan bound");
        }
        pc += len;
    };
    if let Some((slot, _)) = &local {
        let mut scan_pc = to_string_pc + 3;
        while scan_pc < code.len() {
            if builder_local_access(code, scan_pc, true).is_some_and(|(loaded, _)| loaded == *slot)
            {
                return Err("the local builder is used after toString");
            }
            scan_pc += crate::bytecode_analysis::step(code, scan_pc).max(1);
        }
        if local_slot_read_reaches_chain(src, *slot, new_pc, to_string_pc + 3) {
            return Err("the local builder is read before the chain in a loop");
        }
    }
    let local = local.map(|(_, local)| local);
    let chain = SbChain {
        new_pc,
        init_bci: init_pc,
        first_operand_pc,
        constructor_literal,
        to_string_pc,
        parts,
        local,
        has_call,
    };
    // sbf21: the concat bridge serves a recipe of exactly one `boolean`
    // operand and no constant (`"" + b`) the interned "true" / "false" LITERAL
    // (`unary_boolean_concat_operand`, HotSpot's `String.valueOf(boolean)`),
    // while `new StringBuilder().append(b).toString()` is a fresh String: the
    // identity would differ on the decline edge. One more constant segment
    // (a literal or the String constructor's text) makes it an ordinary concat.
    if chain.constructor_literal.is_none()
        && matches!(
            chain.parts.as_slice(),
            [SbChainPart::Value {
                kind: SbAppendKind::Boolean,
                ..
            }]
        )
    {
        return Err("a lone boolean append (the concat bridge would answer an interned literal)");
    }
    if chain.may_be_empty() && !sb_empty_result_fresh_enabled() {
        return Err("the result may be empty (the builder answers the \"\" literal)");
    }
    let end = chain.end_pc();
    for &(start, stop, handler) in src.handlers {
        let covers_all = start <= new_pc && stop >= end;
        let covers_none = stop <= new_pc || start >= end;
        if !(covers_all || covers_none) {
            return Err("an exception-range boundary inside the chain");
        }
        if handler > new_pc && handler < end {
            return Err("an exception handler inside the chain");
        }
    }
    Ok(chain)
}

/// Every chain of the method that [`recognize_sb_chain`] admits AND whose
/// recipe the inline plan admits, in `new` order. Pure. The one predicate
/// both the optimizing tier's builder plan and the String-intrinsic pin use,
/// so the pin releases exactly the chains the builder fuses.
pub fn fusable_sb_chains(
    src: &SbChainCode<'_>,
    invoke_at: &dyn Fn(usize) -> Option<(String, String, String)>,
    ldc_string_at: &dyn Fn(usize) -> Option<Vec<u16>>,
    field_at: &dyn Fn(u16) -> Option<u8>,
) -> Vec<SbChain> {
    let code = src.code;
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == 0xbb {
            if let Ok(chain) = recognize_sb_chain(src, pc, invoke_at, ldc_string_at, field_at) {
                if chain.recipe().plan().is_some() {
                    out.push(chain);
                }
            }
        }
        pc += crate::bytecode_analysis::step(code, pc);
    }
    out
}

/// The reasons [`recognize_sb_chain`] gives for a `new` that is not a
/// `StringBuilder` chain at all (another class, or a constructor shape that
/// cannot be told from another class's): left out of the census.
const NOT_A_BUILDER_CHAIN: [&str; 5] = [
    "not a new",
    "the new is not followed by dup",
    "the new is not followed by dup; invokespecial",
    "unresolved <init>",
    "not a java/lang/StringBuilder",
];

/// sbf21 (the census SBFUSE21-2 / SBFUSE21-3 name as their first step): every
/// `new` of the method that looks like a builder chain and is REFUSED, as
/// `(new pc, reason)`; the reason is [`recognize_sb_chain`]'s `Err` text, or
/// the inline plan's refusal. Pure. A `new StringBuilder(<non-constant>)` is
/// indistinguishable from another class's constructor call here (the recognizer
/// does not resolve the `new`'s class) and is not listed.
pub fn refused_sb_chains(
    src: &SbChainCode<'_>,
    invoke_at: &dyn Fn(usize) -> Option<(String, String, String)>,
    ldc_string_at: &dyn Fn(usize) -> Option<Vec<u16>>,
    field_at: &dyn Fn(u16) -> Option<u8>,
) -> Vec<(usize, &'static str)> {
    let code = src.code;
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == 0xbb {
            match recognize_sb_chain(src, pc, invoke_at, ldc_string_at, field_at) {
                Ok(chain) => {
                    if chain.recipe().plan().is_none() {
                        out.push((pc, "the inline concat plan does not admit the recipe"));
                    }
                }
                Err(reason) => {
                    if !NOT_A_BUILDER_CHAIN.contains(&reason) {
                        out.push((pc, reason));
                    }
                }
            }
        }
        pc += crate::bytecode_analysis::step(code, pc);
    }
    out
}

/// The builder's plan for one fused chain: the chain and the concat bridge
/// site the VM made of its recipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbChainFusion {
    pub chain: SbChain,
    pub site: usize,
}

/// The builder's state while it walks a fused chain
/// (`IrBuilder::sb_chain_fuse_step`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbChainActive {
    /// The chain's `new` pc (its key in the builder's fusion table).
    pub new_pc: usize,
    /// The node standing in the builder's stack slot (never read: nothing
    /// in the chain consumes the builder but the chain itself).
    pub placeholder: crate::ir::NodeId,
    /// The abstract stack depth with the placeholder on top.
    pub base_len: usize,
    /// The run-time operands taken so far, in order.
    pub args: Vec<crate::ir::NodeId>,
    /// The next part to meet.
    pub next_part: usize,
    /// The frame at `new_pc`; operand traps replay the unfused chain from here.
    pub resume_snapshot: u32,
    /// First graph node emitted since the previous chain instruction.
    pub node_cursor: usize,
    /// True if the chain contains a call operand (uses virtual-object frame state).
    pub has_call: bool,
}

#[cfg(test)]
mod tests;
