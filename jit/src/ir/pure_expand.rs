// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT j27 wave 2, lane INTRINS: call sites the optimizing tier EXPANDS into
//! plain IR arithmetic, built only from nodes every pass already knows (`And`,
//! `Or`, `Xor`, the shifts, `Neg`, `Cmp`, `I2L` and the existing
//! [`ScalarOp`] bit moves). No new lowering, no new `ScalarOp`: the expansion
//! is the JDK 25 body's own arithmetic, so the answer is the body's bit for
//! bit, and every optimisation the IR runs (folding, GVN, range analysis, the
//! register allocator) sees ordinary nodes.
//!
//! Why these rows (`docs/known-issues/jit/j27-intrinsic-coverage-vs-hotspot-20261028.md`):
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
#[path = "pure_expand_int_compare.rs"]
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

    /// The raw-bits compare a classification row ends in (`|bits| cc INF`),
    /// with its width; `None` for the other families.
    fn fp_class(self) -> Option<(CmpOp, FpBits)> {
        Some(match self {
            PureExpansion::IsNaND => (CmpOp::Gt, FpBits::DOUBLE),
            PureExpansion::IsNaNF => (CmpOp::Gt, FpBits::FLOAT),
            PureExpansion::IsInfiniteD => (CmpOp::Eq, FpBits::DOUBLE),
            PureExpansion::IsInfiniteF => (CmpOp::Eq, FpBits::FLOAT),
            PureExpansion::IsFiniteD => (CmpOp::Lt, FpBits::DOUBLE),
            PureExpansion::IsFiniteF => (CmpOp::Lt, FpBits::FLOAT),
            _ => return None,
        })
    }
}

/// `CRATONVM_JIT_IR_INTEGER_REVERSE_EXPAND` (j27 wave 2, lane INTRINS;
/// default ON, `=0` kills): `Integer.reverse` / `Long.reverse`.
pub fn ir_integer_reverse_expand_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_INTEGER_REVERSE_EXPAND")
}

/// `CRATONVM_JIT_IR_MATH_COPYSIGN_EXPAND` (j27 wave 2, lane INTRINS; default
/// ON, `=0` kills): `Math.copySign`, both widths.
pub fn ir_math_copysign_expand_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_MATH_COPYSIGN_EXPAND")
}

/// `CRATONVM_JIT_IR_MATH_SIGNUM_EXPAND` (j27 wave 2, lane INTRINS; default
/// ON, `=0` kills): `Math.signum` / `StrictMath.signum`, both widths.
pub fn ir_math_signum_expand_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_MATH_SIGNUM_EXPAND")
}

/// `CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND` (j27 wave 6, lane T1BCE; default ON,
/// `=0` kills): `Double` / `Float` `isNaN` / `isInfinite` / `isFinite`.
/// Default ON as the other rows of this module: one exact bit compare in
/// place of a native-funnel call (`--compatible`) or the JDK body's FP
/// compare (`--jdk-only`).
pub fn ir_fp_classify_expand_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND")
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
        PureExpansion::IsNaND
        | PureExpansion::IsNaNF
        | PureExpansion::IsInfiniteD
        | PureExpansion::IsInfiniteF
        | PureExpansion::IsFiniteD
        | PureExpansion::IsFiniteF => {
            let (cc, fp) = expansion.fp_class().expect("a classification row");
            e.fp_class(args[0], fp, cc)
        }
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
    /// classification rows; `cc` from [`PureExpansion::fp_class`]).
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
mod tests {
    use super::*;

    /// The Java value of data node `id` as raw bits: an `int` sign-extended, a
    /// `float` as its 32 bits zero-extended, a `long` / `double` as 64 bits.
    fn eval(g: &Graph, params: &[i64], id: NodeId) -> i64 {
        let n = &g.nodes[id as usize];
        let x = |k: usize| eval(g, params, n.inputs[k]);
        let wide = n.ty == IrType::Long;
        let raw = match n.op {
            Op::Const(c) => c,
            Op::Param(i) => params[i as usize],
            Op::And => x(0) & x(1),
            Op::Or => x(0) | x(1),
            Op::Xor => x(0) ^ x(1),
            Op::Neg => x(0).wrapping_neg(),
            Op::I2L => x(0),
            Op::Shl if wide => x(0).wrapping_shl(x(1) as u32),
            Op::Shl => i64::from((x(0) as i32).wrapping_shl(x(1) as u32)),
            Op::UShr if wide => ((x(0) as u64).wrapping_shr(x(1) as u32)) as i64,
            Op::UShr => i64::from((x(0) as u32).wrapping_shr(x(1) as u32)),
            Op::Cmp(CmpOp::Eq) => i64::from(x(0) == x(1)),
            Op::Cmp(CmpOp::Gt) => i64::from(x(0) > x(1)),
            Op::Cmp(CmpOp::Lt) => i64::from(x(0) < x(1)),
            Op::ScalarIntrinsic(ScalarOp::ReverseBytesI) => i64::from((x(0) as i32).swap_bytes()),
            Op::ScalarIntrinsic(ScalarOp::ReverseBytesL) => x(0).swap_bytes(),
            Op::ScalarIntrinsic(
                ScalarOp::DoubleToRawLongBits
                | ScalarOp::LongBitsToDouble
                | ScalarOp::FloatToRawIntBits
                | ScalarOp::IntBitsToFloat,
            ) => x(0),
            ref op => panic!("unexpected {op:?} at n{id}"),
        };
        match n.ty {
            // Casts: the Java width of the node's value.
            IrType::Int => i64::from(raw as i32),
            IrType::Float => i64::from(raw as u32),
            _ => raw,
        }
    }

    /// A fresh graph holding `expansion` over parameters of type `ty`;
    /// returns it with the result node.
    fn expanded(expansion: PureExpansion, ty: IrType) -> (Graph, NodeId) {
        let mut g = Graph::empty(0);
        let args: Vec<NodeId> = (0..expansion.arity())
            .map(|i| g.add(Op::Param(i as u16), ty, vec![], None))
            .collect();
        let r = expand(&mut g, expansion, &args, 7);
        (g, r)
    }

    fn ref_copy_sign64(m: u64, s: u64) -> u64 {
        if s >> 63 == 1 {
            m | 1 << 63
        } else {
            m & !(1 << 63)
        }
    }

    fn ref_copy_sign32(m: u32, s: u32) -> u32 {
        if s >> 31 == 1 {
            m | 1 << 31
        } else {
            m & !(1 << 31)
        }
    }

    fn ref_signum64(b: u64) -> u64 {
        let d = f64::from_bits(b);
        if d == 0.0 || d.is_nan() {
            b
        } else if b >> 63 == 1 {
            0xBFF0_0000_0000_0000
        } else {
            0x3FF0_0000_0000_0000
        }
    }

    fn ref_signum32(b: u32) -> u32 {
        let f = f32::from_bits(b);
        if f == 0.0 || f.is_nan() {
            b
        } else if b >> 31 == 1 {
            0xBF80_0000
        } else {
            0x3F80_0000
        }
    }

    const INTS: [i32; 9] = [
        0,
        1,
        -1,
        i32::MIN,
        i32::MAX,
        0x1234_5678,
        0x0F0F_0F0F,
        0x8000_0001_u32 as i32,
        0x0000_8000,
    ];
    const LONGS: [i64; 8] = [
        0,
        1,
        -1,
        i64::MIN,
        i64::MAX,
        0x0123_4567_89AB_CDEF,
        0x8000_0000_0000_0001_u64 as i64,
        0x0000_0001_0000_0000,
    ];
    /// Zeros, ones, infinities, quiet / signalling / negative NaNs with
    /// payloads, the smallest subnormal and the largest finite value.
    const DOUBLES: [u64; 14] = [
        0x0000_0000_0000_0000,
        0x8000_0000_0000_0000,
        0x3FF0_0000_0000_0000,
        0xC000_0000_0000_0000,
        0x7FF0_0000_0000_0000,
        0xFFF0_0000_0000_0000,
        0x7FF8_0000_0000_0000,
        0xFFF8_0000_0000_0123,
        0x7FF0_0000_0000_0001,
        0xFFF4_0000_0000_0000,
        0x0000_0000_0000_0001,
        0x8000_0000_0000_0001,
        0x7FEF_FFFF_FFFF_FFFF,
        0x4045_0000_0000_0000,
    ];
    const FLOATS: [u32; 14] = [
        0x0000_0000,
        0x8000_0000,
        0x3F80_0000,
        0xC000_0000,
        0x7F80_0000,
        0xFF80_0000,
        0x7FC0_0000,
        0xFFC0_0123,
        0x7F80_0001,
        0xFFA0_0000,
        0x0000_0001,
        0x8000_0001,
        0x7F7F_FFFF,
        0x4228_0000,
    ];

    #[test]
    fn reverse_expansions_answer_reverse_bits_on_edge_inputs() {
        let (g, r) = expanded(PureExpansion::ReverseI, IrType::Int);
        for v in INTS {
            let want = i64::from(v.reverse_bits());
            assert_eq!(eval(&g, &[i64::from(v)], r), want, "reverse({v:#x})");
        }
        let (g, r) = expanded(PureExpansion::ReverseL, IrType::Long);
        for v in LONGS {
            assert_eq!(eval(&g, &[v], r), v.reverse_bits(), "reverse({v:#x}L)");
        }
    }

    #[test]
    fn copy_sign_expansions_copy_the_sign_bit_and_keep_nan_payloads() {
        let (g, r) = expanded(PureExpansion::CopySignD, IrType::Double);
        for m in DOUBLES {
            for s in DOUBLES {
                let got = eval(&g, &[m as i64, s as i64], r) as u64;
                assert_eq!(got, ref_copy_sign64(m, s), "copySign({m:#x}, {s:#x})");
            }
        }
        let (g, r) = expanded(PureExpansion::CopySignF, IrType::Float);
        for m in FLOATS {
            for s in FLOATS {
                let got = eval(&g, &[i64::from(m), i64::from(s)], r) as u32;
                assert_eq!(got, ref_copy_sign32(m, s), "copySign({m:#x}f, {s:#x}f)");
            }
        }
    }

    #[test]
    fn signum_expansions_keep_zeros_and_nans_and_answer_unit_otherwise() {
        let (g, r) = expanded(PureExpansion::SignumD, IrType::Double);
        for b in DOUBLES {
            assert_eq!(eval(&g, &[b as i64], r) as u64, ref_signum64(b), "signum({b:#x})");
        }
        let (g, r) = expanded(PureExpansion::SignumF, IrType::Float);
        for b in FLOATS {
            assert_eq!(eval(&g, &[i64::from(b)], r) as u32, ref_signum32(b), "signum({b:#x}f)");
        }
    }

    #[test]
    fn every_expansion_builds_its_anchor_exactly_once() {
        for (expansion, ty) in [
            (PureExpansion::ReverseI, IrType::Int),
            (PureExpansion::ReverseL, IrType::Long),
            (PureExpansion::CopySignD, IrType::Double),
            (PureExpansion::CopySignF, IrType::Float),
            (PureExpansion::SignumD, IrType::Double),
            (PureExpansion::SignumF, IrType::Float),
        ] {
            let (g, r) = expanded(expansion, ty);
            let anchor = Op::ScalarIntrinsic(expansion.anchor());
            let anchors: Vec<usize> = (0..g.nodes.len())
                .filter(|&i| g.nodes[i].op == anchor)
                .collect();
            assert_eq!(anchors, vec![r as usize], "{expansion:?}: the result is the one anchor");
            assert_eq!(g.nodes[r as usize].ty, ty, "{expansion:?}: result type");
        }
    }

    /// `static <T> T f(<T> a[, <T> b]) { return C.m(a[, b]); }` built by the
    /// real builder with the site recorded at pc of the `invokestatic`.
    fn built(expansion: PureExpansion, ty: IrType) -> (Graph, NodeId) {
        let (load, ret, width) = match ty {
            IrType::Int => (0x1a, 0xac, 1),
            IrType::Long => (0x1e, 0xad, 2),
            IrType::Float => (0x22, 0xae, 1),
            _ => (0x26, 0xaf, 2),
        };
        let n = expansion.arity();
        let mut code: Vec<u8> = (0..n).map(|i| load + (i * width) as u8).collect();
        let pc = code.len();
        code.extend_from_slice(&[0xb8, 0x00, 0x01, ret]);
        let len = code.len();
        code.extend_from_slice(&[0, 0]);
        let mut builder = IrBuilder::new(n, n * width);
        builder.set_param_types(&vec![ty; n]);
        builder.set_pure_expansions(HashMap::from([(pc, expansion)]));
        let g = builder.build(&code, len).expect("IR build");
        let rets: Vec<&super::super::Node> =
            g.nodes.iter().filter(|n| n.op == Op::Return).collect();
        assert_eq!(rets.len(), 1, "one return");
        let value = *rets[0].inputs.last().expect("a returned value");
        (g, value)
    }

    #[test]
    fn the_builder_expands_a_recorded_site_with_its_operands_in_order() {
        let (g, r) = built(PureExpansion::CopySignD, IrType::Double);
        assert!(
            !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
            "the site is arithmetic, not a call"
        );
        // copySign(-2.0, +3.0) = +2.0 and copySign(+1.0, -0.0) = -1.0: a
        // swapped operand pair answers -3.0 / +0.0 instead.
        let got = eval(&g, &[0xC000_0000_0000_0000_u64 as i64, 0x4008_0000_0000_0000], r);
        assert_eq!(got as u64, 0x4000_0000_0000_0000);
        let got = eval(&g, &[0x3FF0_0000_0000_0000, 0x8000_0000_0000_0000_u64 as i64], r);
        assert_eq!(got as u64, 0xBFF0_0000_0000_0000);
        let (g, r) = built(PureExpansion::CopySignF, IrType::Float);
        let got = eval(&g, &[0xC000_0000, 0x4040_0000], r);
        assert_eq!(got as u32, 0x4000_0000, "copySign(-2f, 3f)");
        let (g, r) = built(PureExpansion::ReverseL, IrType::Long);
        assert_eq!(eval(&g, &[1], r), i64::MIN, "Long.reverse(1L)");
        let (g, r) = built(PureExpansion::SignumF, IrType::Float);
        assert_eq!(eval(&g, &[0xC2C8_0000], r) as u32, 0xBF80_0000, "signum(-100f)");
    }

    /// `f(bits a[, bits b])`: each argument through `longBitsToDouble` /
    /// `intBitsToFloat` (FP families), the expanded site, the result back
    /// through `doubleToRawLongBits` / `floatToRawIntBits`, built, optionally
    /// optimized, scheduled and LOWERED to machine code.
    #[cfg(target_arch = "x86_64")]
    fn compiled(expansion: PureExpansion, wide: bool, optimize: bool) -> crate::CompiledMethod {
        let fp = !matches!(expansion, PureExpansion::ReverseI | PureExpansion::ReverseL);
        // `lload_0` / `lreturn` or `iload_0` / `ireturn`, and the slot width.
        let (load, ret, slot): (u8, u8, usize) = if wide {
            (0x1e, 0xad, 2)
        } else {
            (0x1a, 0xac, 1)
        };
        let (to_fp, to_bits) = if wide {
            (ScalarOp::LongBitsToDouble, ScalarOp::DoubleToRawLongBits)
        } else {
            (ScalarOp::IntBitsToFloat, ScalarOp::FloatToRawIntBits)
        };
        let n = expansion.arity();
        let mut code: Vec<u8> = Vec::new();
        let mut scalars = HashMap::new();
        let call = |code: &mut Vec<u8>| {
            let pc = code.len();
            // Cast: a small distinct constant-pool index per site.
            code.extend_from_slice(&[0xb8, 0x00, pc as u8 + 1]);
            pc
        };
        for i in 0..n {
            // Cast: `?load_0 + slot`, a one-byte opcode.
            code.push(load + (i * slot) as u8);
            if fp {
                scalars.insert(call(&mut code), to_fp);
            }
        }
        let site = call(&mut code);
        if fp {
            scalars.insert(call(&mut code), to_bits);
        }
        code.push(ret);
        let len = code.len();
        code.extend_from_slice(&[0, 0]);
        let mut builder = IrBuilder::new(n, n * slot);
        let bits_ty = if wide { IrType::Long } else { IrType::Int };
        builder.set_param_types(&vec![bits_ty; n]);
        builder.set_scalar_intrinsics(scalars);
        builder.set_pure_expansions(HashMap::from([(site, expansion)]));
        let mut graph = builder.build(&code, len).expect("IR build");
        if optimize {
            crate::ir_optimize::optimize(&mut graph);
        }
        let schedule = crate::ir_schedule::schedule(&graph);
        let helpers = crate::JitRuntimeHelpers::default();
        crate::ir_lower::lower(&graph, &schedule, n, n * slot, &helpers)
            .unwrap_or_else(|| panic!("{expansion:?} lowers (optimize={optimize})"))
    }

    /// The same edge inputs through the real lowering, unoptimized and
    /// optimized: what a compiled caller actually computes.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn lowered_expansions_compute_the_jdk_answers() {
        for optimize in [false, true] {
            let call = |cm: &crate::CompiledMethod, args: &[i64]| {
                // SAFETY: a straight-line `f(int|long[, int|long])` body this
                // test lowered with an all-zero helper table; it calls nothing.
                unsafe { cm.try_call(args) }.expect("call")
            };
            let cm = compiled(PureExpansion::ReverseI, false, optimize);
            for v in INTS {
                // Cast: this tier's `int` result is the low 32 bits.
                let got = call(&cm, &[i64::from(v)]) as i32;
                assert_eq!(got, v.reverse_bits(), "reverse({v:#x}) optimize={optimize}");
            }
            let cm = compiled(PureExpansion::ReverseL, true, optimize);
            for v in LONGS {
                assert_eq!(call(&cm, &[v]), v.reverse_bits(), "reverse({v:#x}L)");
            }
            let cm = compiled(PureExpansion::SignumD, true, optimize);
            for b in DOUBLES {
                let got = call(&cm, &[b as i64]) as u64;
                assert_eq!(got, ref_signum64(b), "signum({b:#x}) optimize={optimize}");
            }
            let cm = compiled(PureExpansion::SignumF, false, optimize);
            for b in FLOATS {
                let got = call(&cm, &[i64::from(b as i32)]) as u32;
                assert_eq!(got, ref_signum32(b), "signum({b:#x}f) optimize={optimize}");
            }
            let cm = compiled(PureExpansion::CopySignD, true, optimize);
            for m in DOUBLES {
                for s in DOUBLES {
                    let got = call(&cm, &[m as i64, s as i64]) as u64;
                    assert_eq!(got, ref_copy_sign64(m, s), "copySign({m:#x}, {s:#x})");
                }
            }
            let cm = compiled(PureExpansion::CopySignF, false, optimize);
            for m in FLOATS {
                for s in FLOATS {
                    let got = call(&cm, &[i64::from(m as i32), i64::from(s as i32)]) as u32;
                    assert_eq!(got, ref_copy_sign32(m, s), "copySign({m:#x}f, {s:#x}f)");
                }
            }
        }
    }

    #[test]
    fn rows_match_only_their_triples_and_each_switch_kills_its_family() {
        let on = |c, m, d| try_ir_pure_expansion(c, m, d);
        if !super::super::ir_scalar_intrinsics_enabled() {
            assert_eq!(on("java/lang/Integer", "reverse", "(I)I"), None);
            return;
        }
        assert_eq!(on("java/lang/Integer", "reverse", "(I)I"), Some(PureExpansion::ReverseI));
        assert_eq!(on("java/lang/Long", "reverse", "(J)J"), Some(PureExpansion::ReverseL));
        assert_eq!(on("java/lang/Math", "copySign", "(DD)D"), Some(PureExpansion::CopySignD));
        assert_eq!(on("java/lang/Math", "copySign", "(FF)F"), Some(PureExpansion::CopySignF));
        assert_eq!(on("java/lang/StrictMath", "signum", "(D)D"), Some(PureExpansion::SignumD));
        assert_eq!(on("java/lang/Math", "signum", "(F)F"), Some(PureExpansion::SignumF));
        // `StrictMath.copySign` treats a NaN sign as positive: not this body.
        assert_eq!(on("java/lang/StrictMath", "copySign", "(DD)D"), None);
        assert_eq!(on("java/lang/Integer", "reverseBytes", "(I)I"), None);
        assert_eq!(on("java/lang/Short", "reverse", "(S)S"), None);
        for (key, class, method, desc) in [
            ("CRATONVM_JIT_IR_INTEGER_REVERSE_EXPAND", "java/lang/Long", "reverse", "(J)J"),
            ("CRATONVM_JIT_IR_MATH_COPYSIGN_EXPAND", "java/lang/Math", "copySign", "(FF)F"),
            ("CRATONVM_JIT_IR_MATH_SIGNUM_EXPAND", "java/lang/Math", "signum", "(D)D"),
        ] {
            cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
                assert_eq!(on(class, method, desc), None, "{key}=0");
                if !key.contains("REVERSE") {
                    let other = on("java/lang/Integer", "reverse", "(I)I");
                    assert!(other.is_some(), "{key}=0 kills only its family");
                }
            });
        }
    }

    // ── j27 wave 6 (lane T1BCE): `Double` / `Float` classification rows ──

    const DOUBLE_CLASS: [PureExpansion; 3] = [
        PureExpansion::IsNaND,
        PureExpansion::IsInfiniteD,
        PureExpansion::IsFiniteD,
    ];
    const FLOAT_CLASS: [PureExpansion; 3] = [
        PureExpansion::IsNaNF,
        PureExpansion::IsInfiniteF,
        PureExpansion::IsFiniteF,
    ];

    /// JDK 25 `Double.isNaN` / `isInfinite` / `isFinite` of the bits `b`, as
    /// an `int` 0 / 1.
    fn ref_class64(e: PureExpansion, b: u64) -> i64 {
        let d = f64::from_bits(b);
        i64::from(match e {
            PureExpansion::IsNaND => d.is_nan(),
            PureExpansion::IsInfiniteD => d.is_infinite(),
            PureExpansion::IsFiniteD => d.is_finite(),
            other => panic!("not a double classification row: {other:?}"),
        })
    }

    /// The `Float` twin of [`ref_class64`].
    fn ref_class32(e: PureExpansion, b: u32) -> i64 {
        let f = f32::from_bits(b);
        i64::from(match e {
            PureExpansion::IsNaNF => f.is_nan(),
            PureExpansion::IsInfiniteF => f.is_infinite(),
            PureExpansion::IsFiniteF => f.is_finite(),
            other => panic!("not a float classification row: {other:?}"),
        })
    }

    #[test]
    fn classification_expansions_answer_the_jdk_predicates_on_edge_inputs() {
        for e in DOUBLE_CLASS {
            let (g, r) = expanded(e, IrType::Double);
            for b in DOUBLES {
                assert_eq!(eval(&g, &[b as i64], r), ref_class64(e, b), "{e:?}({b:#x})");
            }
        }
        for e in FLOAT_CLASS {
            let (g, r) = expanded(e, IrType::Float);
            for b in FLOATS {
                assert_eq!(eval(&g, &[i64::from(b)], r), ref_class32(e, b), "{e:?}({b:#x}f)");
            }
        }
    }

    #[test]
    fn classification_expansions_read_the_bits_once_and_answer_an_int() {
        for (e, ty) in DOUBLE_CLASS
            .iter()
            .map(|&e| (e, IrType::Double))
            .chain(FLOAT_CLASS.iter().map(|&e| (e, IrType::Float)))
        {
            let (g, r) = expanded(e, ty);
            let anchor = Op::ScalarIntrinsic(e.anchor());
            let anchors = g.nodes.iter().filter(|n| n.op == anchor).count();
            assert_eq!(anchors, 1, "{e:?}: one raw-bits read carries a first-call guard");
            assert!(matches!(g.nodes[r as usize].op, Op::Cmp(_)), "{e:?}: ends in a compare");
            assert_eq!(g.nodes[r as usize].ty, IrType::Int, "{e:?}: a boolean is an int");
        }
    }

    #[test]
    fn classification_rows_match_their_static_triples_and_their_switch_kills_them() {
        let on = |c, m, d| try_ir_pure_expansion(c, m, d);
        if !super::super::ir_scalar_intrinsics_enabled() {
            assert_eq!(on("java/lang/Double", "isNaN", "(D)Z"), None);
            return;
        }
        assert_eq!(on("java/lang/Double", "isNaN", "(D)Z"), Some(PureExpansion::IsNaND));
        assert_eq!(on("java/lang/Float", "isNaN", "(F)Z"), Some(PureExpansion::IsNaNF));
        assert_eq!(on("java/lang/Double", "isInfinite", "(D)Z"), Some(PureExpansion::IsInfiniteD));
        assert_eq!(on("java/lang/Float", "isInfinite", "(F)Z"), Some(PureExpansion::IsInfiniteF));
        assert_eq!(on("java/lang/Double", "isFinite", "(D)Z"), Some(PureExpansion::IsFiniteD));
        assert_eq!(on("java/lang/Float", "isFinite", "(F)Z"), Some(PureExpansion::IsFiniteF));
        // The boxes' instance `isNaN()Z` / `isInfinite()Z` read a field: not
        // these rows.
        assert_eq!(on("java/lang/Double", "isNaN", "()Z"), None);
        assert_eq!(on("java/lang/Float", "isInfinite", "()Z"), None);
        assert_eq!(on("java/lang/Float", "isNaN", "(D)Z"), None);
        let key = "CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND";
        cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
            assert_eq!(on("java/lang/Double", "isNaN", "(D)Z"), None, "{key}=0");
            assert_eq!(on("java/lang/Float", "isFinite", "(F)Z"), None, "{key}=0");
            let other = on("java/lang/Math", "signum", "(D)D");
            assert!(other.is_some(), "{key}=0 kills only its rows");
        });
    }

    /// `static boolean f(bits b) { return C.m(longBitsToDouble(b)); }` (or
    /// the `float` twin): the bits through the scalar row, the expanded site,
    /// `ireturn`; built, optionally optimized, scheduled and LOWERED.
    #[cfg(target_arch = "x86_64")]
    fn compiled_class(
        expansion: PureExpansion,
        wide: bool,
        optimize: bool,
    ) -> crate::CompiledMethod {
        let (load, slot, to_fp, bits_ty) = if wide {
            (0x1e_u8, 2usize, ScalarOp::LongBitsToDouble, IrType::Long)
        } else {
            (0x1a_u8, 1usize, ScalarOp::IntBitsToFloat, IrType::Int)
        };
        // pc 0 `?load_0`, pc 1 `invokestatic #1` (bits -> fp), pc 4
        // `invokestatic #4` (the site), pc 7 `ireturn`.
        let mut code: Vec<u8> = vec![load, 0xb8, 0x00, 0x01, 0xb8, 0x00, 0x04, 0xac];
        let len = code.len();
        code.extend_from_slice(&[0, 0]);
        let mut builder = IrBuilder::new(1, slot);
        builder.set_param_types(&[bits_ty]);
        builder.set_scalar_intrinsics(HashMap::from([(1usize, to_fp)]));
        builder.set_pure_expansions(HashMap::from([(4usize, expansion)]));
        let mut graph = builder.build(&code, len).expect("IR build");
        if optimize {
            crate::ir_optimize::optimize(&mut graph);
        }
        let schedule = crate::ir_schedule::schedule(&graph);
        let helpers = crate::JitRuntimeHelpers::default();
        crate::ir_lower::lower(&graph, &schedule, 1, slot, &helpers)
            .unwrap_or_else(|| panic!("{expansion:?} lowers (optimize={optimize})"))
    }

    /// The edge inputs through the real lowering, unoptimized and optimized.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn lowered_classification_expansions_compute_the_jdk_answers() {
        for optimize in [false, true] {
            let call = |cm: &crate::CompiledMethod, arg: i64| {
                // SAFETY: a straight-line `f(int|long)` body this test lowered
                // with an all-zero helper table; it calls nothing.
                let raw = unsafe { cm.try_call(&[arg]) }.expect("call");
                // Cast: this tier's `int` result is the low 32 bits.
                i64::from(raw as i32)
            };
            for e in DOUBLE_CLASS {
                let cm = compiled_class(e, true, optimize);
                for b in DOUBLES {
                    let want = ref_class64(e, b);
                    assert_eq!(call(&cm, b as i64), want, "{e:?}({b:#x}) optimize={optimize}");
                }
            }
            for e in FLOAT_CLASS {
                let cm = compiled_class(e, false, optimize);
                for b in FLOATS {
                    let got = call(&cm, i64::from(b as i32));
                    assert_eq!(got, ref_class32(e, b), "{e:?}({b:#x}f) optimize={optimize}");
                }
            }
        }
    }
}
