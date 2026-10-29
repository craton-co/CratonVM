// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT j27 wave 4, lane LATIN1 (`docs/known-issues/jit/j27-latin1-character-class-expand-proposal-20261028.md`):
//! the optimizing tier answers `Character.isDigit` / `isLetter` /
//! `isLetterOrDigit` / `isWhitespace` / `isUpperCase` / `isLowerCase`, both
//! the `(C)Z` and the `(I)Z` overload, inline for a Latin-1 code point.
//!
//! ```text
//! cp >>> 8 == 0 ?  (BITMAP[cp >>> 6] >>> (cp & 63)) & 1   -- the JDK table
//!               :  Character.isX(cp)                       -- the call, as before
//! ```
//!
//! # Why it is exact
//!
//! JDK 25's `Character.isX(int)` is `CharacterData.of(cp).isX(cp)` (or, for
//! `isLetter` / `isLetterOrDigit`, a type mask over `getType(cp)`), and
//! `CharacterData.of` picks `CharacterDataLatin1` exactly when `cp >>> 8 ==
//! 0` -- the test this expansion makes, so a negative or a non-Latin-1 code
//! point always takes the call. The `(C)Z` overloads are `isX((int) ch)`.
//! Over Latin-1 the answers are the bodies of `CharacterDataLatin1` over its
//! own `A[]` / `B[]` property tables, which this module carries verbatim
//! ([`CHARACTER_DATA_LATIN1_A`], [`CHARACTER_DATA_LATIN1_B`], extracted
//! mechanically from Temurin 25.0.3's `src.zip`) and evaluates with those
//! bodies ([`Latin1CharClass::jdk_answer`]). The unit tests check every one
//! of the 256 answers of every predicate against independent references (the
//! Rust `char` Unicode properties and the `Character.isWhitespace` contract)
//! and the counts the JDK tables give.
//!
//! The hit arm writes nothing, so the miss call runs from the memory state
//! before the expansion; the shape is `try_reference_get_intrinsic`'s
//! (an `If`, the call on the miss edge, a `Merge` and value / memory `Phi`s).
//! A code point the build proves Latin-1 (a constant in `[0, 255]`, or an
//! `& c` with such a constant mask -- the `b & 0xFF` of a byte loop) gets the
//! table answer with no branch and no call.
//!
//! Default OFF (`CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND=1` turns it on),
//! under `CRATONVM_JIT_IR_SCALAR_INTRINSICS` like `pure_expand`: unmeasured
//! (the proposal asks for instructions per op on `R24RegexSplitRows` before a
//! flip, and the memory note "intrinsic loses to bytecode" applies).
//! Declined (the site is built exactly as before): the switch off, a
//! first-call site, a site the planner gave no `invoke_info` row, an
//! unloaded-static-call trap pc, a caller-held synchronized row, a top-level
//! pc with no frame state, a splice pc outside the innermost body, a
//! non-`Int` argument, dead control.

use super::pure_expand::Emit;
use super::{CmpOp, Graph, IrBuilder, IrType, NodeId, Op};
use crate::JitInvokeInfo;

/// One `java/lang/Character` classification predicate this tier answers
/// inline over Latin-1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Latin1CharClass {
    /// `Character.isDigit`.
    Digit,
    /// `Character.isLetter`.
    Letter,
    /// `Character.isLetterOrDigit`.
    LetterOrDigit,
    /// `Character.isWhitespace`.
    Whitespace,
    /// `Character.isUpperCase`.
    UpperCase,
    /// `Character.isLowerCase`.
    LowerCase,
}

/// `Character.UPPERCASE_LETTER` .. `OTHER_LETTER` (types 1 ..= 5): the mask
/// JDK 25's `Character.isLetter(int)` shifts by `getType(cp)`.
const LETTER_TYPES: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5);
/// `Character.DECIMAL_DIGIT_NUMBER`, the extra bit of `isLetterOrDigit(int)`.
const DECIMAL_DIGIT_NUMBER: u32 = 9;

impl Latin1CharClass {
    /// Every predicate, in declaration order.
    pub const ALL: [Latin1CharClass; 6] = [
        Latin1CharClass::Digit,
        Latin1CharClass::Letter,
        Latin1CharClass::LetterOrDigit,
        Latin1CharClass::Whitespace,
        Latin1CharClass::UpperCase,
        Latin1CharClass::LowerCase,
    ];

    /// The `java/lang/Character` method name.
    pub fn method_name(self) -> &'static str {
        match self {
            Latin1CharClass::Digit => "isDigit",
            Latin1CharClass::Letter => "isLetter",
            Latin1CharClass::LetterOrDigit => "isLetterOrDigit",
            Latin1CharClass::Whitespace => "isWhitespace",
            Latin1CharClass::UpperCase => "isUpperCase",
            Latin1CharClass::LowerCase => "isLowerCase",
        }
    }

    /// JDK 25's answer for the Latin-1 code point `ch`: the
    /// `CharacterDataLatin1` (or `Character`) body over the JDK's own tables.
    pub const fn jdk_answer(self, ch: u8) -> bool {
        // Cast: a `u8` always indexes a 256-entry table.
        let props = CHARACTER_DATA_LATIN1_A[ch as usize];
        let props_ex = CHARACTER_DATA_LATIN1_B[ch as usize];
        // `getType(ch)`: `getProperties(ch) & 0x1F`.
        let ty = props & 0x1F;
        match self {
            // `'0' <= ch && ch <= '9'`.
            Latin1CharClass::Digit => ch.is_ascii_digit(),
            Latin1CharClass::Letter => (LETTER_TYPES >> ty) & 1 != 0,
            Latin1CharClass::LetterOrDigit => {
                ((LETTER_TYPES | 1 << DECIMAL_DIGIT_NUMBER) >> ty) & 1 != 0
            }
            // `(props & 0x00007000) == 0x00004000`.
            Latin1CharClass::Whitespace => props & 0x0000_7000 == 0x0000_4000,
            // `(getPropertiesEx(ch) & 0x0002) != 0`.
            Latin1CharClass::UpperCase => props_ex & 0x0002 != 0,
            // `(getPropertiesEx(ch) & 0x0001) != 0`.
            Latin1CharClass::LowerCase => props_ex & 0x0001 != 0,
        }
    }

    /// The 256 answers as a bitmap: word `k` holds code points `64k ..
    /// 64k + 63`, bit `cp & 63`.
    pub const fn bitmap(self) -> [u64; 4] {
        let mut words = [0u64; 4];
        let mut ch = 0usize;
        while ch < 256 {
            // Cast: `ch < 256`.
            if self.jdk_answer(ch as u8) {
                words[ch >> 6] |= 1 << (ch & 63);
            }
            ch += 1;
        }
        words
    }

    fn from_method(method: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.method_name() == method)
    }
}

/// `CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND` (j27 wave 4, lane LATIN1;
/// default OFF, `=1` turns it on). Read per call (no `static`).
pub fn ir_latin1_char_class_expand_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND")
}

/// The predicate of a `java/lang/Character` static call site, if the
/// optimizing tier expands it (both switches on).
pub fn try_ir_latin1_char_class(
    class: &str,
    method: &str,
    descriptor: &str,
) -> Option<Latin1CharClass> {
    if class != "java/lang/Character" || !matches!(descriptor, "(C)Z" | "(I)Z") {
        return None;
    }
    let found = Latin1CharClass::from_method(method)?;
    (super::ir_scalar_intrinsics_enabled() && ir_latin1_char_class_expand_enabled())
        .then_some(found)
}

/// `Int` 1 exactly when `cp >>> 8 == 0`: `CharacterData.of`'s own Latin-1
/// test (a negative `cp` fails it).
fn in_latin1(graph: &mut Graph, cp: NodeId, pc: usize) -> NodeId {
    let mut e = Emit { graph, pc };
    let high = e.with_const(Op::UShr, IrType::Int, cp, 8);
    let zero = e.konst(IrType::Int, 0);
    e.node(Op::Cmp(CmpOp::Eq), IrType::Int, vec![high, zero])
}

/// The JDK answer (`Int` 0 / 1) for a `cp` in `[0, 255]`: bit `cp & 63` of
/// bitmap word `cp >>> 6`, the word selected branch-free
/// (`mask_k & -(index == k)` over the non-empty words). Meaningless for any
/// other `cp`, so it is only ever used behind [`in_latin1`] or a proof.
fn table_answer(graph: &mut Graph, class: Latin1CharClass, cp: NodeId, pc: usize) -> NodeId {
    let mut e = Emit { graph, pc };
    let index = e.with_const(Op::UShr, IrType::Int, cp, 6);
    let mut word: Option<NodeId> = None;
    for (k, mask) in class.bitmap().into_iter().enumerate() {
        if mask == 0 {
            continue;
        }
        // Cast: `k < 4`; `mask` is stored as its bit pattern.
        let is_k = e.with_const(Op::Cmp(CmpOp::Eq), IrType::Int, index, k as i64);
        let wide = e.node(Op::I2L, IrType::Long, vec![is_k]);
        let all_ones = e.node(Op::Neg, IrType::Long, vec![wide]);
        let term = e.with_const(Op::And, IrType::Long, all_ones, mask as i64);
        word = Some(match word {
            Some(prev) => e.node(Op::Or, IrType::Long, vec![prev, term]),
            None => term,
        });
    }
    let word = word.unwrap_or_else(|| e.konst(IrType::Long, 0));
    let bit = e.with_const(Op::And, IrType::Int, cp, 63);
    let shifted = e.node(Op::UShr, IrType::Long, vec![word, bit]);
    let low = e.node(Op::L2I, IrType::Int, vec![shifted]);
    e.with_const(Op::And, IrType::Int, low, 1)
}

/// Is `cp` provably in `[0, 255]` at build time: a constant there, or an
/// `And` with such a constant operand?
fn provably_latin1(graph: &Graph, cp: NodeId) -> bool {
    let latin1_const = |id: NodeId| {
        matches!(graph.node_opt(id).map(|n| &n.op), Some(&Op::Const(c)) if (0..=255).contains(&c))
    };
    let Some(n) = graph.node_opt(cp) else {
        return false;
    };
    match n.op {
        Op::Const(_) => latin1_const(cp),
        Op::And => n.inputs.as_slice().iter().any(|&i| latin1_const(i)),
        _ => false,
    }
}

impl IrBuilder {
    /// Expand the `Character.isX` call at `pc` (see the module doc); `true`
    /// when it consumed the site. A refusal leaves the stack untouched.
    pub(super) fn try_emit_latin1_char_class(
        &mut self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        code_len: usize,
    ) -> bool {
        let Some(class) = self.latin1_char_class_site(pc, info_ptr, num_args, ret_type, code_len)
        else {
            return false;
        };
        let cp = self.pop();
        let proven = provably_latin1(&self.graph, cp);
        let value = if proven {
            table_answer(&mut self.graph, class, cp, pc)
        } else {
            self.latin1_guarded_call(class, cp, pc, info_ptr, code_len)
        };
        self.push(value);
        if super::ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] Character.{} Latin-1 table inline at bytecode pc {pc} in {} ({})",
                class.method_name(),
                self.method_label.as_deref().unwrap_or("<unknown>"),
                if proven { "proven Latin-1, no call" } else { "call on the non-Latin-1 arm" },
            );
        }
        true
    }

    /// The predicate of the site at `pc` when this build may expand it.
    fn latin1_char_class_site(
        &self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        code_len: usize,
    ) -> Option<Latin1CharClass> {
        if info_ptr == 0 || num_args != 1 || ret_type != b'Z' {
            return None;
        }
        // SAFETY: a non-zero `invoke_info` pointer addresses the
        // `JitInvokeInfo` the planner (`lib.rs`'s invoke-planning loop) leaked
        // for this site and keeps alive on the artifact, as
        // `try_reference_get_intrinsic`'s read of the same row.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.invoke_kind != 3 {
            return None;
        }
        let class = try_ir_latin1_char_class(info.class_name, info.method_name, info.descriptor)?;
        let arg_is_int = self
            .stack
            .last()
            .and_then(|&v| self.graph.node_opt(v))
            .is_some_and(|n| n.ty == IrType::Int);
        (arg_is_int && self.latin1_site_admits(pc, code_len)).then_some(class)
    }

    /// `try_reference_get_intrinsic`'s admission of a site whose miss edge is
    /// a plain call: live control, not an unloaded-static trap pc, and either
    /// inside the innermost splice body or a top-level pc with its own frame
    /// state and no caller-held synchronized row.
    fn latin1_site_admits(&self, pc: usize, code_len: usize) -> bool {
        if self.ctrl_opt().is_none() || self.unloaded_static_call_pcs.contains(&pc) {
            return false;
        }
        match self.splice.last() {
            Some(frame) => pc >= frame.base && pc < frame.end,
            None => {
                pc < code_len
                    && !self.sync_direct_calls.contains_key(&pc)
                    && self.has_frame_state_at(pc)
            }
        }
    }

    /// `If(cp >>> 8 == 0)`: the table answer on the true edge, the site's own
    /// call on the false edge, joined; returns the value `Phi`.
    fn latin1_guarded_call(
        &mut self,
        class: Latin1CharClass,
        cp: NodeId,
        pc: usize,
        info_ptr: usize,
        code_len: usize,
    ) -> NodeId {
        let mem0 = self.mem;
        let in_range = in_latin1(&mut self.graph, cp, pc);
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![self.ctrl, in_range], Some(pc));
        let hit = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let miss = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        let answer = table_answer(&mut self.graph, class, cp, pc);
        // The miss edge: the call this site always made, from the memory
        // state before the expansion (the hit path wrote nothing).
        self.ctrl = miss;
        let call = self.graph.add(
            Op::Call { info_ptr },
            IrType::Int,
            vec![miss, mem0, cp],
            Some(pc),
        );
        self.note_splice_call(call);
        self.mem = call;
        self.plant_call_handler_entry(pc, call, code_len);
        let join = self
            .graph
            .add(Op::Merge, IrType::Control, vec![hit, self.ctrl], Some(pc));
        let mem = self
            .graph
            .add(Op::Phi, IrType::Memory, vec![join, mem0, self.mem], Some(pc));
        let result = self
            .graph
            .add(Op::Phi, IrType::Int, vec![join, answer, call], Some(pc));
        self.ctrl = join;
        self.mem = mem;
        self.latin1_post_call_join(call, result);
        result
    }

    /// As the `Reference.get` / `ThreadLocal` expansions' miss call: inside a
    /// splice the post-call chain snapshot names the call's result in place of
    /// the join's `Phi`, or the call gets none.
    fn latin1_post_call_join(&mut self, call: NodeId, result: NodeId) {
        if self.splice.is_empty() || !self.pending_post_call.is_some_and(|(c, _)| c == call) {
            return;
        }
        if super::ir_nested_miss_post_call_exit_enabled() {
            self.post_call_join_value = Some((result, call));
        } else {
            self.pending_post_call = None;
        }
    }
}

/// JDK 25 `java.lang.CharacterDataLatin1.A[]` (Temurin 25.0.3+9 `src.zip`,
/// the generated `CharacterDataLatin1.java`), 8 entries per row, as the
/// `int` bit patterns. Copied by a script, not typed: never edit by hand.
pub const CHARACTER_DATA_LATIN1_A: [u32; 256] = [
    0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F,
    0x4800100F, 0x5800400F, 0x5000400F, 0x5800400F, 0x6000400F, 0x5000400F, 0x4800100F, 0x4800100F,
    0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F,
    0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x5000400F, 0x5000400F, 0x5000400F, 0x5800400F,
    0x6000400C, 0x68000018, 0x68000018, 0x28000018, 0x2800601A, 0x28000018, 0x68000018, 0x68000018,
    0xE8000015, 0xE8000016, 0x68000018, 0x20000019, 0x38000018, 0x20000014, 0x38000018, 0x38000018,
    0x18003609, 0x18003609, 0x18003609, 0x18003609, 0x18003609, 0x18003609, 0x18003609, 0x18003609,
    0x18003609, 0x18003609, 0x38000018, 0x68000018, 0xE8000019, 0x68000019, 0xE8000019, 0x68000018,
    0x68000018, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1,
    0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1,
    0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1, 0x00827FE1,
    0x00827FE1, 0x00827FE1, 0x00827FE1, 0xE8000015, 0x68000018, 0xE8000016, 0x6800001B, 0x68005017,
    0x6800001B, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2,
    0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2,
    0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2, 0x00817FE2,
    0x00817FE2, 0x00817FE2, 0x00817FE2, 0xE8000015, 0x68000019, 0xE8000016, 0x68000019, 0x4800100F,
    0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x5000100F, 0x4800100F, 0x4800100F,
    0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F,
    0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F,
    0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F, 0x4800100F,
    0x3800000C, 0x68000018, 0x2800601A, 0x2800601A, 0x2800601A, 0x2800601A, 0x6800001C, 0x68000018,
    0x6800001B, 0x6800001C, 0x00007005, 0xE800001D, 0x68000019, 0x48001010, 0x6800001C, 0x6800001B,
    0x2800001C, 0x28000019, 0x1800060B, 0x1800060B, 0x6800001B, 0x07FD7002, 0x68000018, 0x68000018,
    0x6800001B, 0x1800050B, 0x00007005, 0xE800001E, 0x6800080B, 0x6800080B, 0x6800080B, 0x68000018,
    0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001,
    0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001,
    0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x68000019,
    0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x00827001, 0x07FD7002,
    0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002,
    0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002,
    0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x68000019,
    0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x00817002, 0x061D7002,
];

/// JDK 25 `java.lang.CharacterDataLatin1.B[]` (the `getPropertiesEx` table;
/// same source and rule as [`CHARACTER_DATA_LATIN1_A`]).
pub const CHARACTER_DATA_LATIN1_B: [u16; 256] = [
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0440, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0440, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0460, 0x0460, 0x0460, 0x0460, 0x0460, 0x0460, 0x0460, 0x0460,
    0x0460, 0x0460, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032,
    0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032,
    0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032,
    0x0032, 0x0032, 0x0032, 0x0000, 0x0000, 0x0000, 0x0000, 0x0020,
    0x0000, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031,
    0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031,
    0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031,
    0x0031, 0x0031, 0x0031, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0840, 0x0031, 0x0000, 0x0000, 0x0000, 0x0840, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0031, 0x0000, 0x0020,
    0x0000, 0x0000, 0x0031, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032,
    0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032,
    0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0000,
    0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0032, 0x0031,
    0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031,
    0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031,
    0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0000,
    0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031, 0x0031,
];

#[cfg(test)]
#[path = "latin1_char_class_tests.rs"]
mod tests;
