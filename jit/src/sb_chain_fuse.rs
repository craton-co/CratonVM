// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An explicit `new StringBuilder().append(..)...toString()` chain fused into
//! one concatenation: JIT round 21 wave 20, lane sbfuse, STRINTR21-4
//! (`docs/internal/fixed-bugs/jr21-strintr2-sb-chunk-builder-chain-is-not-fused-FIXED-20261027.md`;
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
//! `new java/lang/StringBuilder; dup; invokespecial <init>()V`, then zero
//! or more `invokevirtual append(X)` with `X` one of `String`, `char`, `int`,
//! `long`, each preceded by operand code that pushes exactly that one value
//! from locals, constants and side-effect-free arithmetic (no call, no field
//! or array access, no division, no store, no branch), then `invokevirtual
//! toString()`. The builder is never stored, passed, compared or locked: it
//! lives only on the operand stack, consumed by the next append. Refused:
//! a builder kept in a local (a loop-carried builder), `append(Object)` /
//! `append(CharSequence)` / `append(float)` / ..., any other builder member
//! (`setLength`, `insert`, `length`, `charAt`, ...), a branch target or an
//! exception-handler boundary inside the chain, and a recipe the inline plan
//! does not admit (`JitConcatInlinePlan::from_recipe`).

use crate::concat_inline_plan::{JitConcatInlinePlan, CONCAT_TAG_ARG, CONCAT_TAG_CONST};

/// `CRATONVM_JIT_IR_SB_EMPTY_RESULT_FRESH` (CONF21-2, page
/// `jr21-conf-string-identity-differs-only-from-hotspot-c2-FIXED-20261027.md`; HotSpot
/// conformance, default ON, `=0` refuses a chain whose result may be empty, as
/// before, so it answers the `""` literal in every tier). Read per compile.
pub fn sb_empty_result_fresh_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SB_EMPTY_RESULT_FRESH")
}

/// The builder class the fusion serves.
pub const SB_CLASS: &str = "java/lang/StringBuilder";

/// Most instructions one operand may take (a bound on what the scan reads,
/// not a semantic limit).
pub const SB_CHAIN_MAX_OPERAND_INSNS: usize = 12;

/// Most bytes from the `new` to the `toString()` (a bound on the scan).
pub const SB_CHAIN_MAX_BYTES: usize = 1024;

/// `CRATONVM_JIT_IR_SB_CHAIN_FUSE` (JIT round 21 wave 20, lane sbfuse,
/// STRINTR21-4; perf, default ON, `=0` builds every chain as before and puts
/// its builder sites back under the String-intrinsic pin). Read per compile,
/// no process global.
pub fn sb_chain_fuse_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SB_CHAIN_FUSE")
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
}

impl SbAppendKind {
    /// The kind of an `append` with this descriptor, or `None` for every
    /// overload the fusion does not render (`Object`, `CharSequence`,
    /// `char[]`, `boolean`, `float`, `double`, `StringBuffer`, ...).
    pub fn from_append_descriptor(descriptor: &str) -> Option<Self> {
        match descriptor {
            "(Ljava/lang/String;)Ljava/lang/StringBuilder;" => Some(Self::String),
            "(C)Ljava/lang/StringBuilder;" => Some(Self::Char),
            "(I)Ljava/lang/StringBuilder;" => Some(Self::Int),
            "(J)Ljava/lang/StringBuilder;" => Some(Self::Long),
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
        }
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
    /// The `invokevirtual toString()`.
    pub to_string_pc: usize,
    /// Every `append`, in order.
    pub parts: Vec<SbChainPart>,
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
    /// The `invokespecial <init>()V`.
    pub fn init_pc(&self) -> usize {
        self.new_pc + 4
    }

    /// The first pc after `new; dup; invokespecial <init>()V`.
    pub fn first_operand_pc(&self) -> usize {
        self.new_pc + 7
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
        let mut recipe = Vec::with_capacity(self.parts.len());
        let mut constants = Vec::new();
        let mut descriptor = String::from("(");
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
fn pure_operand_effect(code: &[u8], pc: usize) -> Option<(usize, usize, usize)> {
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
        _ => return None,
    })
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
) -> Result<SbChain, &'static str> {
    let code = src.code;
    if code.get(new_pc) != Some(&0xbb) {
        return Err("not a new");
    }
    if code.get(new_pc + 3) != Some(&0x59) || code.get(new_pc + 4) != Some(&0xb7) {
        return Err("the new is not followed by dup; invokespecial");
    }
    let init_pc = new_pc + 4;
    let (class, name, descriptor) = invoke_at(init_pc).ok_or("unresolved <init>")?;
    if class != SB_CLASS || name != "<init>" {
        return Err("not a java/lang/StringBuilder");
    }
    if descriptor != "()V" {
        return Err("a StringBuilder constructor with arguments");
    }
    let is_target = |pc: usize| src.branch_targets.get(pc).copied().unwrap_or(false);
    if is_target(new_pc + 3) || is_target(init_pc) {
        return Err("a branch target inside the chain");
    }
    let mut parts: Vec<SbChainPart> = Vec::new();
    let mut pc = init_pc + 3;
    let mut operand_pc = pc;
    // Values the current operand's code has pushed and not consumed.
    let mut depth = 0usize;
    let mut insns = 0usize;
    let mut operand_has_ldc = false;
    let to_string_pc = loop {
        if pc >= code.len() || pc - new_pc > SB_CHAIN_MAX_BYTES {
            return Err("the chain runs past the scan bound");
        }
        if is_target(pc) {
            return Err("a branch target inside the chain");
        }
        let op = code[pc];
        if op == 0xb6 {
            let (class, name, descriptor) =
                invoke_at(pc).ok_or("unresolved invokevirtual in the chain")?;
            if class != SB_CLASS {
                return Err("a call that is not the builder's inside the chain");
            }
            if name == "toString" && descriptor == "()Ljava/lang/String;" && depth == 0 {
                break pc;
            }
            if name != "append" {
                return Err(
                    "a builder member other than append / toString (setLength, insert, ...)",
                );
            }
            let kind = SbAppendKind::from_append_descriptor(&descriptor).ok_or(
                "an append overload the concat does not render (Object, CharSequence, ...)",
            )?;
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
            operand_pc = pc;
            depth = 0;
            insns = 0;
            operand_has_ldc = false;
            continue;
        }
        let (pops, pushes, len) = if let Some(len) = ldc_len(op) {
            operand_has_ldc = true;
            (0, 1, len)
        } else {
            pure_operand_effect(code, pc)
                .ok_or("operand code outside the pure set (a call, a field, a store, ...)")?
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
    let chain = SbChain {
        new_pc,
        to_string_pc,
        parts,
    };
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
) -> Vec<SbChain> {
    let code = src.code;
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == 0xbb {
            if let Ok(chain) = recognize_sb_chain(src, pc, invoke_at, ldc_string_at) {
                if chain.recipe().plan().is_some() {
                    out.push(chain);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const SB_INIT: u16 = 1;
    const APPEND_S: u16 = 2;
    const APPEND_C: u16 = 3;
    const APPEND_I: u16 = 4;
    const APPEND_J: u16 = 5;
    const TO_STRING: u16 = 6;
    const APPEND_O: u16 = 7;
    const SET_LENGTH: u16 = 8;
    const INSERT: u16 = 9;
    const OTHER_CALL: u16 = 10;
    const OTHER_INIT: u16 = 11;
    const SB_INIT_STR: u16 = 12;

    fn member(cp: u16) -> Option<(String, String, String)> {
        let (c, n, d) = match cp {
            SB_INIT => (SB_CLASS, "<init>", "()V"),
            SB_INIT_STR => (SB_CLASS, "<init>", "(Ljava/lang/String;)V"),
            APPEND_S => (
                SB_CLASS,
                "append",
                "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
            ),
            APPEND_C => (SB_CLASS, "append", "(C)Ljava/lang/StringBuilder;"),
            APPEND_I => (SB_CLASS, "append", "(I)Ljava/lang/StringBuilder;"),
            APPEND_J => (SB_CLASS, "append", "(J)Ljava/lang/StringBuilder;"),
            APPEND_O => (
                SB_CLASS,
                "append",
                "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
            ),
            TO_STRING => (SB_CLASS, "toString", "()Ljava/lang/String;"),
            SET_LENGTH => (SB_CLASS, "setLength", "(I)V"),
            INSERT => (SB_CLASS, "insert", "(IC)Ljava/lang/StringBuilder;"),
            OTHER_CALL => ("java/lang/Integer", "intValue", "()I"),
            OTHER_INIT => ("java/lang/Object", "<init>", "()V"),
            _ => return None,
        };
        Some((c.to_string(), n.to_string(), d.to_string()))
    }

    /// A tiny assembler: `new SB(cp 20); dup; invokespecial <init>`.
    struct Asm(Vec<u8>);

    impl Asm {
        fn new() -> Self {
            Asm(Vec::new())
        }
        fn op(mut self, bytes: &[u8]) -> Self {
            self.0.extend_from_slice(bytes);
            self
        }
        fn invoke(self, opcode: u8, cp: u16) -> Self {
            let [hi, lo] = cp.to_be_bytes();
            self.op(&[opcode, hi, lo])
        }
        fn sb_new(self) -> Self {
            self.op(&[0xbb, 0, 20, 0x59]).invoke(0xb7, SB_INIT)
        }
        fn append(self, cp: u16) -> Self {
            self.invoke(0xb6, cp)
        }
        fn sb_to_string(self) -> Self {
            self.invoke(0xb6, TO_STRING)
        }
    }

    fn recognize_at(
        code: &[u8],
        new_pc: usize,
        handlers: &[(usize, usize, usize)],
    ) -> Result<SbChain, &'static str> {
        let targets = crate::bytecode_analysis::branch_target_map(code, code.len());
        let src = SbChainCode {
            code,
            branch_targets: &targets,
            handlers,
        };
        let invoke_at = |pc: usize| member(u16::from_be_bytes([code[pc + 1], code[pc + 2]]));
        // `ldc #30` is the String "k", `ldc #31` an int constant.
        let ldc_string_at = |pc: usize| match code[pc] {
            0x12 if code[pc + 1] == 30 => Some("k".encode_utf16().collect()),
            0x13 if code[pc + 2] == 30 => Some("k".encode_utf16().collect()),
            _ => None,
        };
        recognize_sb_chain(&src, new_pc, &invoke_at, &ldc_string_at)
    }

    fn recognize(code: &[u8]) -> Result<SbChain, &'static str> {
        recognize_at(code, 0, &[])
    }

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// `R21StrintrRows.sbChunk`'s chain:
    /// `new SB().append("k").append(i).append('-').append(i * 7).toString()`.
    fn sb_chunk() -> Vec<u8> {
        Asm::new()
            .sb_new()
            .op(&[0x12, 30]) // ldc "k"
            .append(APPEND_S)
            .op(&[0x1b]) // iload_1
            .append(APPEND_I)
            .op(&[0x10, 45]) // bipush '-'
            .append(APPEND_C)
            .op(&[0x1b, 0x10, 7, 0x68]) // iload_1; bipush 7; imul
            .append(APPEND_I)
            .sb_to_string()
            .op(&[0xb0]) // areturn
            .0
    }

    #[test]
    fn the_sb_chunk_chain_is_one_recipe_with_folded_literals() {
        let code = sb_chunk();
        let chain = recognize(&code).expect("admitted");
        assert_eq!(chain.new_pc, 0);
        assert_eq!(chain.parts.len(), 4);
        assert_eq!(
            chain.parts[0],
            SbChainPart::Literal {
                text: units("k"),
                operand_pc: 7,
                append_pc: 9
            }
        );
        assert_eq!(
            chain.parts[1],
            SbChainPart::Value {
                kind: SbAppendKind::Int,
                operand_pc: 12,
                append_pc: 13
            }
        );
        assert_eq!(
            chain.parts[2],
            SbChainPart::Literal {
                text: units("-"),
                operand_pc: 16,
                append_pc: 18
            }
        );
        assert_eq!(
            chain.parts[3],
            SbChainPart::Value {
                kind: SbAppendKind::Int,
                operand_pc: 21,
                append_pc: 25
            }
        );
        assert_eq!(chain.to_string_pc, 28);
        assert_eq!(chain.end_pc(), 31);
        assert_eq!(chain.value_count(), 2);
        assert_eq!(chain.invoke_pcs(), vec![4, 9, 13, 18, 25, 28]);
        let r = chain.recipe();
        assert_eq!(
            r.recipe,
            vec![
                CONCAT_TAG_CONST,
                CONCAT_TAG_ARG,
                CONCAT_TAG_CONST,
                CONCAT_TAG_ARG
            ]
        );
        assert_eq!(r.constants, vec![units("k"), units("-")]);
        assert_eq!(r.descriptor, "(II)Ljava/lang/String;");
        let plan = r.plan().expect("the inline plan admits it");
        assert_eq!(plan.arg_count, 2);
        assert_eq!(plan.literal_len, 2);
        assert!(!plan.wide);
        assert!(chain.contains_pc(9) && chain.contains_pc(30));
        assert!(!chain.contains_pc(0) && !chain.contains_pc(31));
    }

    #[test]
    fn every_admitted_operand_kind_and_constant_folds() {
        let code = Asm::new()
            .sb_new()
            .op(&[0x2b]) // aload_1 (a String)
            .append(APPEND_S)
            .op(&[0x1c, 0x10, 0x61, 0x60, 0x92]) // (char) (iload_2 + 'a')
            .append(APPEND_C)
            .op(&[0x20]) // lload_2
            .append(APPEND_J)
            .op(&[0x1b, 0x85, 0x0a, 0x61]) // (long) i + 1L
            .append(APPEND_J)
            .op(&[0x0a]) // lconst_1
            .append(APPEND_J)
            .op(&[0x11, 0x01, 0x00]) // sipush 256 as int
            .append(APPEND_I)
            .op(&[0x11, 0x04, 0x30]) // sipush 0x430 as char (UTF16 literal)
            .append(APPEND_C)
            .op(&[0x02]) // iconst_m1 as int
            .append(APPEND_I)
            .sb_to_string()
            .0;
        let chain = recognize(&code).expect("admitted");
        let kinds: Vec<String> = chain
            .parts
            .iter()
            .map(|p| match p {
                SbChainPart::Literal { text, .. } => {
                    format!("lit:{}", String::from_utf16_lossy(text))
                }
                SbChainPart::Value { kind, .. } => format!("{kind:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "String",
                "Char",
                "Long",
                "Long",
                "lit:1",
                "lit:256",
                "lit:\u{430}",
                "lit:-1"
            ]
        );
        let r = chain.recipe();
        assert_eq!(r.descriptor, "(Ljava/lang/String;CJJ)Ljava/lang/String;");
        // A literal above 0xFF: a wide plan (UTF16 result), still admitted.
        assert!(r.plan().expect("admitted").wide);
    }

    #[test]
    fn a_ldc_w_literal_and_a_chain_mid_method_are_admitted() {
        let code = Asm::new()
            .op(&[0x1a, 0x57]) // iload_0; pop  (code before the chain)
            .sb_new()
            .op(&[0x13, 0, 30]) // ldc_w "k"
            .append(APPEND_S)
            .op(&[0x1a])
            .append(APPEND_I)
            .sb_to_string()
            .op(&[0xb0])
            .0;
        let chain = recognize_at(&code, 2, &[]).expect("admitted");
        assert_eq!(chain.new_pc, 2);
        assert!(matches!(chain.parts[0], SbChainPart::Literal { .. }));
        // The same code under a handler that covers all of it, or none of it.
        assert!(recognize_at(&code, 2, &[(0, code.len(), code.len() - 1)]).is_ok());
        assert!(recognize_at(&code, 2, &[(0, 2, code.len() - 1)]).is_ok());
    }

    #[test]
    fn an_escaping_builder_is_refused() {
        // foo(new SB().append(i)): the builder is passed after an append.
        let passed = Asm::new()
            .sb_new()
            .op(&[0x1a])
            .append(APPEND_I)
            .invoke(0xb8, OTHER_CALL)
            .0;
        assert!(recognize(&passed).is_err());
        // The builder stored in a local after <init> (the loop-carried shape:
        // `StringBuilder b = new StringBuilder(); for (..) b.append(x);`).
        let stored = Asm::new()
            .sb_new()
            .op(&[0x4c]) // astore_1
            .op(&[0x2b, 0x1a]) // aload_1; iload_0
            .append(APPEND_I)
            .op(&[0x57]) // pop
            .op(&[0x2b])
            .sb_to_string()
            .0;
        assert!(recognize(&stored).is_err());
        // Stored after an append (`b = new SB().append(i); ...`).
        let stored_late = Asm::new()
            .sb_new()
            .op(&[0x1a])
            .append(APPEND_I)
            .op(&[0x4c])
            .0;
        assert!(recognize(&stored_late).is_err());
        // Duplicated (compared, locked, or passed while kept).
        let dup = Asm::new()
            .sb_new()
            .op(&[0x59]) // dup
            .op(&[0xc2]) // monitorenter
            .0;
        assert!(recognize(&dup).is_err());
        // An operand that reads the builder from below its own values
        // (`swap` would put the builder on top).
        let swap = Asm::new()
            .sb_new()
            .op(&[0x1a, 0x5f]) // iload_0; swap
            .0;
        assert!(recognize(&swap).is_err());
    }

    #[test]
    fn a_loop_carried_builder_is_refused() {
        // `new SB()` then a backward branch into the chain region.
        let code = Asm::new()
            .sb_new()
            .op(&[0x1a]) // pc 7: iload_0 (a loop head)
            .append(APPEND_I)
            .op(&[0xa7, 0xff, 0xfc]) // pc 11: goto 7
            .0;
        assert_eq!(
            recognize(&code).unwrap_err(),
            "a branch target inside the chain"
        );
        // A branch target on the toString of an otherwise fine chain.
        let mut code = Asm::new()
            .sb_new()
            .op(&[0x1a])
            .append(APPEND_I)
            .sb_to_string()
            .op(&[0xb0])
            .0;
        let ts = 11;
        code.extend_from_slice(&[0xa7]);
        let back = (ts as i16 - code.len() as i16 + 1).to_be_bytes();
        code.extend_from_slice(&back);
        assert_eq!(
            recognize(&code).unwrap_err(),
            "a branch target inside the chain"
        );
    }

    #[test]
    fn an_object_append_and_other_builder_members_are_refused() {
        let object = Asm::new()
            .sb_new()
            .op(&[0x2b])
            .append(APPEND_O)
            .sb_to_string()
            .0;
        assert!(recognize(&object).unwrap_err().contains("append overload"));
        let set_length = Asm::new()
            .sb_new()
            .op(&[0x1a])
            .append(APPEND_I)
            .op(&[0x03])
            .invoke(0xb6, SET_LENGTH)
            .0;
        assert!(recognize(&set_length)
            .unwrap_err()
            .contains("other than append"));
        let insert = Asm::new()
            .sb_new()
            .op(&[0x03, 0x10, 0x41])
            .invoke(0xb6, INSERT)
            .sb_to_string()
            .0;
        assert!(recognize(&insert).is_err());
        // An operand that is a call.
        let call = Asm::new()
            .sb_new()
            .op(&[0x2b])
            .invoke(0xb6, OTHER_CALL)
            .append(APPEND_I)
            .sb_to_string()
            .0;
        assert!(recognize(&call).unwrap_err().contains("not the builder's"));
        // A field read, a division, an array load, a store inside an operand.
        for operand in [
            &[0x2a, 0xb4, 0, 5][..], // aload_0; getfield
            &[0x1a, 0x1b, 0x6c],     // iload_0; iload_1; idiv
            &[0x2b, 0x1a, 0x2e],     // aload_1; iload_0; iaload
            &[0x84, 0, 1, 0x1a],     // iinc 0 1; iload_0
        ] {
            let code = Asm::new()
                .sb_new()
                .op(operand)
                .append(APPEND_I)
                .sb_to_string()
                .0;
            assert!(recognize(&code).is_err(), "{operand:?}");
        }
    }

    #[test]
    fn other_constructors_classes_and_empty_results_are_refused() {
        let with_arg = Asm::new()
            .op(&[0xbb, 0, 20, 0x59, 0x2b])
            .invoke(0xb7, SB_INIT_STR)
            .0;
        assert!(recognize(&with_arg).is_err());
        let other = Asm::new()
            .op(&[0xbb, 0, 20, 0x59])
            .invoke(0xb7, OTHER_INIT)
            .0;
        assert_eq!(
            recognize(&other).unwrap_err(),
            "not a java/lang/StringBuilder"
        );
        // No dup (`new; invokespecial` then the reference is dropped).
        let no_dup = Asm::new().op(&[0xbb, 0, 20]).invoke(0xb7, SB_INIT).0;
        assert!(recognize(&no_dup).is_err());
        // Under `CRATONVM_JIT_IR_SB_EMPTY_RESULT_FRESH=0` a chain that may come
        // out empty answers the builder's "" literal (the default fuses it;
        // see `a_chain_that_may_be_empty_is_c2s_fresh_string`): zero appends,
        // or only String operands.
        let zero = Asm::new().sb_new().sb_to_string().0;
        let strings = Asm::new()
            .sb_new()
            .op(&[0x2b])
            .append(APPEND_S)
            .op(&[0x2c])
            .append(APPEND_S)
            .sb_to_string()
            .0;
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_SB_EMPTY_RESULT_FRESH", Some("0"))],
            || {
                assert!(recognize(&zero).unwrap_err().contains("empty"));
                assert!(recognize(&strings).unwrap_err().contains("empty"));
            },
        );
        // ... but not with a non-empty literal beside them.
        let with_literal = Asm::new()
            .sb_new()
            .op(&[0x2b])
            .append(APPEND_S)
            .op(&[0x12, 30])
            .append(APPEND_S)
            .sb_to_string()
            .0;
        assert!(recognize(&with_literal).is_ok());
        // An ldc that is not a String literal (an int `ldc` appended as int).
        let int_ldc = Asm::new()
            .sb_new()
            .op(&[0x12, 31])
            .append(APPEND_I)
            .sb_to_string()
            .0;
        assert!(recognize(&int_ldc).is_err());
    }

    #[test]
    fn a_chain_across_a_try_boundary_is_refused() {
        let code = sb_chunk();
        // A range that starts inside the chain.
        assert!(recognize_at(&code, 0, &[(9, 40, 30)]).is_err());
        // A range that ends inside the chain.
        assert!(recognize_at(&code, 0, &[(0, 13, 30)]).is_err());
        // A handler inside the chain.
        assert!(recognize_at(&code, 0, &[(40, 50, 13)]).is_err());
    }

    #[test]
    fn the_method_scan_returns_only_fusable_chains() {
        // A fusable chain, then a refused one (Object append).
        let mut code = sb_chunk();
        code.pop(); // drop the areturn
        code.push(0x57); // pop
        let second = code.len();
        let tail = Asm::new()
            .sb_new()
            .op(&[0x2b])
            .append(APPEND_O)
            .sb_to_string()
            .op(&[0xb0])
            .0;
        code.extend_from_slice(&tail);
        let targets = crate::bytecode_analysis::branch_target_map(&code, code.len());
        let src = SbChainCode {
            code: &code,
            branch_targets: &targets,
            handlers: &[],
        };
        let c = code.clone();
        let invoke_at = move |pc: usize| member(u16::from_be_bytes([c[pc + 1], c[pc + 2]]));
        let ldc_string_at = |_pc: usize| Some(units("k"));
        let chains = fusable_sb_chains(&src, &invoke_at, &ldc_string_at);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].new_pc, 0);
        assert!(chains.iter().all(|c| c.new_pc != second));
    }

    /// CONF21-2 (page `jr21-conf-string-identity-differs-only-from-hotspot-c2-FIXED-20261027.md`):
    /// a chain whose result may be empty is fused, so it answers a FRESH
    /// String, as C2's `OptimizeStringConcat` does (`R20QStrIdentity
    /// identity-warm` bit 9): `new StringBuilder().toString()` becomes the
    /// empty recipe, and a chain of String operands the ordinary one.
    #[test]
    fn a_chain_that_may_be_empty_is_c2s_fresh_string() {
        let zero = Asm::new().sb_new().sb_to_string().op(&[0xb0]).0;
        let strings = Asm::new()
            .sb_new()
            .op(&[0x2b])
            .append(APPEND_S)
            .op(&[0x2c])
            .append(APPEND_S)
            .sb_to_string()
            .0;
        let (chain, strings) = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_SB_EMPTY_RESULT_FRESH", None)],
            || (recognize(&zero), recognize(&strings)),
        );
        let chain = chain.expect("the zero-append chain is fused by default");
        assert_eq!(strings.expect("String operands are fused by default").parts.len(), 2);
        assert!(chain.parts.is_empty());
        assert_eq!(chain.to_string_pc, 7);
        assert_eq!(chain.invoke_pcs(), vec![4, 7]);
        let recipe = chain.recipe();
        assert!(recipe.recipe.is_empty());
        assert!(recipe.constants.is_empty());
        assert_eq!(recipe.descriptor, "()Ljava/lang/String;");
        assert!(recipe.plan().is_some(), "the inline plan admits the empty recipe");
    }

    #[test]
    fn the_switch_is_on_by_default_and_zero_turns_it_off() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_SB_CHAIN_FUSE", None)],
            || assert!(sb_chain_fuse_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_SB_CHAIN_FUSE", Some("0"))],
            || assert!(!sb_chain_fuse_enabled()),
        );
    }
}
