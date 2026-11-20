// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT j27 wave 2, lane INTRINS: call sites the optimizing tier EXPANDS into
//! plain IR arithmetic, built only from nodes every pass already knows (`And`,
//! `Or`, `Xor`, the shifts, `Neg`, `Cmp`, `I2L` and the existing
//! [`ScalarOp`] bit moves). No new lowering, no new `ScalarOp`: the expansion
//! is the JDK 25 body's own arithmetic, so the answer is the body's bit for
//! bit, and every optimisation the IR runs (folding, GVN, range analysis, the
//! register allocator) sees ordinary nodes.
//!
//! Why these rows (`docs/feature-designs/jit-ir-optimizations.md`):
//! HotSpot 25's C2 intrinsifies all three families (`_reverse_i` /
//! `_reverse_l`, `_copySignD` / `_copySignF`, `_dsignum` / `_fsignum`), while
//! here each is a registered `Intrinsic` native over the JDK body, so a
//! compiled caller paid a native-funnel crossing per call -- and a method
//! calling `Integer.reverse` (a single-pass `JitIntrinsic`) was refused from
//! the optimizing tier whole ("loop-shaped or memory call-site intrinsic").
//!
//! * `Integer.reverse(I)I` / `Long.reverse(J)J` -- the three mask-and-shift
//!   steps of JDK 25's body, then its `reverseBytes` ([`ScalarOp::ReverseBytesI`]
//!   / [`ScalarOp::ReverseBytesL`], `BSWAP`).
//! * `Math.copySign(DD)D` / `(FF)F` -- `longBitsToDouble((raw(sign) & SIGN) |
//!   (raw(magnitude) & ~SIGN))`, the JDK body; a NaN keeps its payload, a NaN
//!   SIGN argument gives its own sign bit (HotSpot's `Math` answer). NOT
//!   `StrictMath.copySign`, which treats a NaN sign as positive.
//! * `Math.signum` / `StrictMath.signum` (`D` and `F`) -- `d == 0 || isNaN(d)
//!   ? d : copySign(1.0, d)`, branchless in the raw bits: `keep` is
//!   `|bits| == 0 || |bits| > INF`, widened to an all-ones mask that selects
//!   the argument's own bits (signed zero and NaN payload kept) over `±1.0`.
//! * `Double` / `Float` `isNaN` / `isInfinite` / `isFinite` (j27 wave 6, lane
//!   T1BCE; HotSpot's `_doubleIsInfinite` / `_doubleIsFinite` / `_float*`
//!   rows, and C2's inlined `v != v`) -- one compare of the raw bits'
//!   absolute value against `+Infinity`'s bits: `>` is a NaN (`v != v`), `==`
//!   an infinity (`v == ±INFINITY`), `<` a finite value (`Math.abs(v) <=
//!   MAX_VALUE`). Exact for every bit pattern: the absolute bits are
//!   non-negative, so the signed compare orders them as the magnitudes. In
//!   `--compatible` these six are registered natives (a funnel call per test).
//!
//! Each family has its own default-ON switch with an `=0` kill, and all of
//! them sit under `CRATONVM_JIT_IR_SCALAR_INTRINSICS` like the scalar rows.
//! The planner (`lib.rs`, next to the `try_ir_scalar_intrinsic` site) records
//! a site here and builds no `invoke_info` row; [`IrBuilder::try_emit_pure_expansion`]
//! is the other half, asked at the same point as the scalar rows. A site the
//! compile door found unresolved keeps its one-shot first-execution guard: it
//! rides on the expansion's single [`PureExpansion::anchor`] node through
//! `Graph::scalar_first_call_cells`, exactly as a scalar row's does.

use super::{CmpOp, Graph, IrBuilder, IrType, NodeId, Op, ScalarOp};
use std::collections::HashMap;

// j27 wave 8 (lane W8INTR): `Integer` / `Long` `compareUnsigned` and `signum`.
mod int_compare;

/// One call-site family the optimizing tier expands inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PureExpansion {
    /// `Integer.reverse(int)`.
    ReverseI,
    /// `Long.reverse(long)`.
    ReverseL,
    /// `Math.copySign(double magnitude, double sign)`.
    CopySignD,
    /// `Math.copySign(float magnitude, float sign)`.
    CopySignF,
    /// `Math.signum(double)` / `StrictMath.signum(double)`.
    SignumD,
    /// `Math.signum(float)` / `StrictMath.signum(float)`.
    SignumF,
    /// `Double.isNaN(double)`.
    IsNaND,
    /// `Float.isNaN(float)`.
    IsNaNF,
    /// `Double.isInfinite(double)`.
    IsInfiniteD,
    /// `Float.isInfinite(float)`.
    IsInfiniteF,
    /// `Double.isFinite(double)`.
    IsFiniteD,
    /// `Float.isFinite(float)`.
    IsFiniteF,
    /// `Integer.compareUnsigned(int, int)` (j27 wave 8, [`int_compare`]).
    CompareUnsignedI,
    /// `Long.compareUnsigned(long, long)`.
    CompareUnsignedL,
    /// `Integer.signum(int)`.
    SignumI,
    /// `Long.signum(long)`.
    SignumL,
}

impl PureExpansion {
    /// How many operands the call pops.
    pub fn arity(self) -> usize {
        match self {
            PureExpansion::CopySignD
            | PureExpansion::CopySignF
            | PureExpansion::CompareUnsignedI
            | PureExpansion::CompareUnsignedL => 2,
            _ => 1,
        }
    }

    /// The one `ScalarIntrinsic` node every expansion of this family builds
    /// exactly once (its last step). A first-call site's guard is filed
    /// against it (`ir_lower`'s `Op::ScalarIntrinsic` arm emits the guard for
    /// the node matching `(pc, op)`).
    pub fn anchor(self) -> ScalarOp {
        match self {
            PureExpansion::ReverseI => ScalarOp::ReverseBytesI,
            PureExpansion::ReverseL => ScalarOp::ReverseBytesL,
            PureExpansion::CopySignD | PureExpansion::SignumD => ScalarOp::LongBitsToDouble,
            PureExpansion::CopySignF | PureExpansion::SignumF => ScalarOp::IntBitsToFloat,
            // The classification rows end in a compare: their one scalar node
            // is the raw-bits read of the argument.
            PureExpansion::IsNaND | PureExpansion::IsInfiniteD | PureExpansion::IsFiniteD => {
                ScalarOp::DoubleToRawLongBits
            }
            PureExpansion::IsNaNF | PureExpansion::IsInfiniteF | PureExpansion::IsFiniteF => {
                ScalarOp::FloatToRawIntBits
            }
            // j27 wave 8: one three-way compare, the result.
            PureExpansion::CompareUnsignedI
            | PureExpansion::CompareUnsignedL
            | PureExpansion::SignumI
            | PureExpansion::SignumL => int_compare::compare_op(self),
        }
    }


}

/// `CRATONVM_JIT_IR_INTEGER_REVERSE_EXPAND` (j27 wave 2, lane INTRINS;
/// default ON, `=0` kills): `Integer.reverse` / `Long.reverse`.
pub fn ir_integer_reverse_expand_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_MATH_COPYSIGN_EXPAND` (j27 wave 2, lane INTRINS; default
/// ON, `=0` kills): `Math.copySign`, both widths.
pub fn ir_math_copysign_expand_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_MATH_SIGNUM_EXPAND` (j27 wave 2, lane INTRINS; default
/// ON, `=0` kills): `Math.signum` / `StrictMath.signum`, both widths.
pub fn ir_math_signum_expand_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND` (j27 wave 6, lane T1BCE; default ON,
/// `=0` kills): `Double` / `Float` `isNaN` / `isInfinite` / `isFinite`.
/// Default ON as the other rows of this module: one exact bit compare in
/// place of a native-funnel call (`--compatible`) or the JDK body's FP
/// compare (`--real-jdk`).
pub fn ir_fp_classify_expand_enabled() -> bool {
    true
}

/// The family of a static call site, if the optimizing tier expands it. The
/// planner and the tests ask this; read per call (no `static`).
pub fn try_ir_pure_expansion(class: &str, method: &str, descriptor: &str) -> Option<PureExpansion> {
    if !super::ir_scalar_intrinsics_enabled() {
        return None;
    }
    // j27 wave 8 (lane W8INTR): the unsigned compares and integral signums.
    if let Some(expansion) = int_compare::row(class, method, descriptor) {
        return int_compare::ir_int_compare_expand_enabled().then_some(expansion);
    }
    let (expansion, enabled) = match (class, method, descriptor) {
        ("java/lang/Integer", "reverse", "(I)I") => {
            (PureExpansion::ReverseI, ir_integer_reverse_expand_enabled())
        }
        ("java/lang/Long", "reverse", "(J)J") => {
            (PureExpansion::ReverseL, ir_integer_reverse_expand_enabled())
        }
        ("java/lang/Math", "copySign", "(DD)D") => {
            (PureExpansion::CopySignD, ir_math_copysign_expand_enabled())
        }
        ("java/lang/Math", "copySign", "(FF)F") => {
            (PureExpansion::CopySignF, ir_math_copysign_expand_enabled())
        }
        ("java/lang/Math" | "java/lang/StrictMath", "signum", "(D)D") => {
            (PureExpansion::SignumD, ir_math_signum_expand_enabled())
        }
        ("java/lang/Math" | "java/lang/StrictMath", "signum", "(F)F") => {
            (PureExpansion::SignumF, ir_math_signum_expand_enabled())
        }
        ("java/lang/Double", "isNaN", "(D)Z") => {
            (PureExpansion::IsNaND, ir_fp_classify_expand_enabled())
        }
        ("java/lang/Float", "isNaN", "(F)Z") => {
            (PureExpansion::IsNaNF, ir_fp_classify_expand_enabled())
        }
        ("java/lang/Double", "isInfinite", "(D)Z") => {
            (PureExpansion::IsInfiniteD, ir_fp_classify_expand_enabled())
        }
        ("java/lang/Float", "isInfinite", "(F)Z") => {
            (PureExpansion::IsInfiniteF, ir_fp_classify_expand_enabled())
        }
        ("java/lang/Double", "isFinite", "(D)Z") => {
            (PureExpansion::IsFiniteD, ir_fp_classify_expand_enabled())
        }
        ("java/lang/Float", "isFinite", "(F)Z") => {
            (PureExpansion::IsFiniteF, ir_fp_classify_expand_enabled())
        }
        _ => return None,
    };
    enabled.then_some(expansion)
}

/// Build `expansion` over `args` (operand order, first argument first) at
/// bytecode `pc`; returns the node holding the call's result.
pub fn expand(graph: &mut Graph, expansion: PureExpansion, args: &[NodeId], pc: usize) -> NodeId {
    let mut e = Emit { graph, pc };
    match expansion {
        PureExpansion::IsNaND => e.fp_class(args[0], FpBits::DOUBLE, CmpOp::Gt),
        PureExpansion::IsNaNF => e.fp_class(args[0], FpBits::FLOAT, CmpOp::Gt),
        PureExpansion::IsInfiniteD => e.fp_class(args[0], FpBits::DOUBLE, CmpOp::Eq),
        PureExpansion::IsInfiniteF => e.fp_class(args[0], FpBits::FLOAT, CmpOp::Eq),
        PureExpansion::IsFiniteD => e.fp_class(args[0], FpBits::DOUBLE, CmpOp::Lt),
        PureExpansion::IsFiniteF => e.fp_class(args[0], FpBits::FLOAT, CmpOp::Lt),
        PureExpansion::ReverseI => e.reverse_bits(args[0], IrType::Int),
        PureExpansion::ReverseL => e.reverse_bits(args[0], IrType::Long),
        PureExpansion::CopySignD => e.copy_sign(args[0], args[1], FpBits::DOUBLE),
        PureExpansion::CopySignF => e.copy_sign(args[0], args[1], FpBits::FLOAT),
        PureExpansion::SignumD => e.signum(args[0], FpBits::DOUBLE),
        PureExpansion::SignumF => e.signum(args[0], FpBits::FLOAT),
        PureExpansion::CompareUnsignedI | PureExpansion::CompareUnsignedL => {
            e.compare_unsigned(args[0], args[1], int_compare::operand_type(expansion))
        }
        PureExpansion::SignumI | PureExpansion::SignumL => {
            e.integral_signum(args[0], int_compare::operand_type(expansion))
        }
    }
}

/// The raw-bits view of one floating-point width.
#[derive(Clone, Copy)]
struct FpBits {
    /// `Long` for `double`, `Int` for `float`.
    bits: IrType,
    to_bits: ScalarOp,
    from_bits: ScalarOp,
    /// The sign bit (`int` constants are stored sign-extended).
    sign: i64,
    /// `+Infinity`'s bits: an absolute value above it is a NaN.
    inf: i64,
    /// `+1.0`'s bits.
    one: i64,
}

impl FpBits {
    const DOUBLE: FpBits = FpBits {
        bits: IrType::Long,
        to_bits: ScalarOp::DoubleToRawLongBits,
        from_bits: ScalarOp::LongBitsToDouble,
        sign: i64::MIN,
        inf: 0x7FF0_0000_0000_0000,
        one: 0x3FF0_0000_0000_0000,
    };
    const FLOAT: FpBits = FpBits {
        bits: IrType::Int,
        to_bits: ScalarOp::FloatToRawIntBits,
        from_bits: ScalarOp::IntBitsToFloat,
        sign: i32::MIN as i64,
        inf: 0x7F80_0000,
        one: 0x3F80_0000,
    };
}

/// Appends data nodes at one bytecode pc (also `latin1_char_class`'s).
pub(super) struct Emit<'g> {
    pub(super) graph: &'g mut Graph,
    pub(super) pc: usize,
}

impl Emit<'_> {
    pub(super) fn konst(&mut self, ty: IrType, value: i64) -> NodeId {
        self.graph.add(Op::Const(value), ty, vec![], None)
    }

    pub(super) fn node(&mut self, op: Op, ty: IrType, inputs: Vec<NodeId>) -> NodeId {
        self.graph.add(op, ty, inputs, Some(self.pc))
    }

    pub(super) fn with_const(&mut self, op: Op, ty: IrType, a: NodeId, value: i64) -> NodeId {
        let k = self.konst(ty, value);
        self.node(op, ty, vec![a, k])
    }

    fn scalar(&mut self, op: ScalarOp, a: NodeId) -> NodeId {
        self.node(Op::ScalarIntrinsic(op), op.result_type(), vec![a])
    }

    /// JDK 25 `Integer.reverse` / `Long.reverse`: swap adjacent bits, pairs
    /// and nibbles, then reverse the bytes.
    fn reverse_bits(&mut self, mut x: NodeId, ty: IrType) -> NodeId {
        let (steps, bswap) = if ty == IrType::Long {
            (
                [
                    (0x5555_5555_5555_5555, 1),
                    (0x3333_3333_3333_3333, 2),
                    (0x0F0F_0F0F_0F0F_0F0F, 4),
                ],
                ScalarOp::ReverseBytesL,
            )
        } else {
            (
                [(0x5555_5555, 1), (0x3333_3333, 2), (0x0F0F_0F0F, 4)],
                ScalarOp::ReverseBytesI,
            )
        };
        for (mask, shift) in steps {
            let count = self.konst(IrType::Int, shift);
            let low = self.with_const(Op::And, ty, x, mask);
            let low = self.node(Op::Shl, ty, vec![low, count]);
            let high = self.node(Op::UShr, ty, vec![x, count]);
            let high = self.with_const(Op::And, ty, high, mask);
            x = self.node(Op::Or, ty, vec![low, high]);
        }
        self.scalar(bswap, x)
    }

    /// JDK 25 `Math.copySign(magnitude, sign)`.
    fn copy_sign(&mut self, magnitude: NodeId, sign: NodeId, fp: FpBits) -> NodeId {
        let m = self.scalar(fp.to_bits, magnitude);
        let s = self.scalar(fp.to_bits, sign);
        let s = self.with_const(Op::And, fp.bits, s, fp.sign);
        let m = self.with_const(Op::And, fp.bits, m, !fp.sign);
        let r = self.node(Op::Or, fp.bits, vec![s, m]);
        self.scalar(fp.from_bits, r)
    }

    /// JDK 25 `Math.signum(d)`: `d` itself for a zero or NaN, else
    /// `copySign(1.0, d)`; `one ^ ((bits ^ one) & keep_mask)`.
    fn signum(&mut self, d: NodeId, fp: FpBits) -> NodeId {
        let bits = self.scalar(fp.to_bits, d);
        let keep = self.zero_or_nan(bits, fp);
        let keep = if fp.bits == IrType::Long {
            self.node(Op::I2L, IrType::Long, vec![keep])
        } else {
            keep
        };
        let mask = self.node(Op::Neg, fp.bits, vec![keep]);
        let sign = self.with_const(Op::And, fp.bits, bits, fp.sign);
        let one = self.with_const(Op::Or, fp.bits, sign, fp.one);
        let diff = self.node(Op::Xor, fp.bits, vec![bits, one]);
        let diff = self.node(Op::And, fp.bits, vec![diff, mask]);
        let r = self.node(Op::Xor, fp.bits, vec![one, diff]);
        self.scalar(fp.from_bits, r)
    }

    /// `Int` 1 when `|raw bits of v| cc +Infinity's bits`, else 0 (the
    /// classification rows; `cc` selects the comparison against infinity).
    fn fp_class(&mut self, v: NodeId, fp: FpBits, cc: CmpOp) -> NodeId {
        let bits = self.scalar(fp.to_bits, v);
        let abs = self.with_const(Op::And, fp.bits, bits, !fp.sign);
        let inf = self.konst(fp.bits, fp.inf);
        self.node(Op::Cmp(cc), IrType::Int, vec![abs, inf])
    }

    /// `Int` 1 when `bits` is a signed zero or a NaN, else 0.
    fn zero_or_nan(&mut self, bits: NodeId, fp: FpBits) -> NodeId {
        let abs = self.with_const(Op::And, fp.bits, bits, !fp.sign);
        let zero = self.konst(fp.bits, 0);
        let inf = self.konst(fp.bits, fp.inf);
        let is_zero = self.node(Op::Cmp(CmpOp::Eq), IrType::Int, vec![abs, zero]);
        let is_nan = self.node(Op::Cmp(CmpOp::Gt), IrType::Int, vec![abs, inf]);
        self.node(Op::Or, IrType::Int, vec![is_zero, is_nan])
    }
}

impl IrBuilder {
    /// Record the call sites the planner will expand inline (see the module
    /// doc). Same contract as [`IrBuilder::set_scalar_intrinsics`]: such a
    /// site has no `invoke_info` row.
    pub fn set_pure_expansions(&mut self, sites: HashMap<usize, PureExpansion>) {
        self.pure_expansions = sites;
    }

    /// Expand the call at `pc` if the planner recorded it; `true` when it
    /// consumed the site. Operands come off the abstract stack last first.
    pub(super) fn try_emit_pure_expansion(&mut self, pc: usize) -> bool {
        let Some(&expansion) = self.pure_expansions.get(&pc) else {
            return false;
        };
        let mut args: Vec<NodeId> = (0..expansion.arity()).map(|_| self.pop()).collect();
        args.reverse();
        let node = expand(&mut self.graph, expansion, &args, pc);
        if let Some(cell) = self.first_call_cell_at(pc) {
            self.graph
                .scalar_first_call_cells
                .push((pc, expansion.anchor(), cell));
        }
        self.push(node);
        crate::ir_evidence::note_builder_expansion();
        true
    }
}

#[cfg(test)]
mod tests;
