// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 8, lane W8INTR (`docs/feature-designs/jit-ir-optimizations.md`):
//! the unsigned compares and the integral `signum`s as [`PureExpansion`] rows,
//! each ONE existing three-way compare node ([`ScalarOp::CompareI`] /
//! [`ScalarOp::CompareL`], `CMP; SETG; SETL; SUB`), so no new lowering.
//!
//! * `Integer.compareUnsigned(II)I` / `Long.compareUnsigned(JJ)I` -- HotSpot
//!   25's `_compareUnsigned_i` / `_compareUnsigned_l` (C2 `CmpU3` / `CmpUL3`).
//!   JDK 25 body: `compare(x + MIN_VALUE, y + MIN_VALUE)`; `+ MIN_VALUE` is
//!   `^ MIN_VALUE` in two's complement (only the sign bit changes), so the
//!   expansion is `compare(x ^ MIN, y ^ MIN)`, bit for bit the body.
//! * `Integer.signum(I)I` / `Long.signum(J)I` -- not C2 intrinsics (C2 inlines
//!   the two-shift body), but here registered `Intrinsic` natives in
//!   `--compatible` (`phases_late::register_p71_wrapper_extras`), so a
//!   compiled caller paid a native-funnel crossing (`BigDecimal.signum`'s
//!   `Long.signum(intCompact)`). JDK 25 body: `(i >> 31) | (-i >>> 31)` (and
//!   the `long` twin cast to `int`), which is `compare(i, 0)` for every `i`,
//!   `MIN_VALUE` included (-1).
//!
//! Switch `CRATONVM_JIT_IR_INT_COMPARE_EXPAND` (default OFF, `=1` on; also
//! under `CRATONVM_JIT_IR_SCALAR_INTRINSICS`): opt-in because it is
//! unmeasured, the "intrinsic loses to bytecode" rule. In `--real-jdk` the
//! site is the JDK body (two ALU ops, normally spliced), so the gain is
//! `--compatible`'s funnel call per op.

use super::{Emit, PureExpansion};
use super::super::{IrType, NodeId, Op, ScalarOp};

/// `CRATONVM_JIT_IR_INT_COMPARE_EXPAND` (j27 wave 8, lane W8INTR; default
/// OFF, `=1` on): the four rows of this module.
pub fn ir_int_compare_expand_enabled() -> bool {
    false}

/// This module's family of a static call site, before the switch.
pub(super) fn row(class: &str, method: &str, descriptor: &str) -> Option<PureExpansion> {
    Some(match (class, method, descriptor) {
        ("java/lang/Integer", "compareUnsigned", "(II)I") => PureExpansion::CompareUnsignedI,
        ("java/lang/Long", "compareUnsigned", "(JJ)I") => PureExpansion::CompareUnsignedL,
        ("java/lang/Integer", "signum", "(I)I") => PureExpansion::SignumI,
        ("java/lang/Long", "signum", "(J)I") => PureExpansion::SignumL,
        _ => return None,
    })
}

/// The operand width of a row: `Long` for the `long` rows, else `Int`.
pub(super) fn operand_type(expansion: PureExpansion) -> IrType {
    match expansion {
        PureExpansion::CompareUnsignedL | PureExpansion::SignumL => IrType::Long,
        _ => IrType::Int,
    }
}

/// The three-way compare a row ends in (its anchor).
pub(super) fn compare_op(expansion: PureExpansion) -> ScalarOp {
    if operand_type(expansion) == IrType::Long {
        ScalarOp::CompareL
    } else {
        ScalarOp::CompareI
    }
}

impl Emit<'_> {
    /// `compare(a ^ MIN, b ^ MIN)` at the operands' width.
    pub(super) fn compare_unsigned(&mut self, a: NodeId, b: NodeId, ty: IrType) -> NodeId {
        // `int` constants are stored sign-extended.
        let min = if ty == IrType::Long {
            i64::MIN
        } else {
            i64::from(i32::MIN)
        };
        let a = self.with_const(Op::Xor, ty, a, min);
        let b = self.with_const(Op::Xor, ty, b, min);
        self.three_way(a, b, ty)
    }

    /// `compare(v, 0)`: JDK 25 `Integer.signum` / `Long.signum`.
    pub(super) fn integral_signum(&mut self, v: NodeId, ty: IrType) -> NodeId {
        let zero = self.konst(ty, 0);
        self.three_way(v, zero, ty)
    }

    /// The one `CompareI` / `CompareL` node (an `int` in {-1, 0, 1}).
    fn three_way(&mut self, a: NodeId, b: NodeId, ty: IrType) -> NodeId {
        let op = if ty == IrType::Long {
            ScalarOp::CompareL
        } else {
            ScalarOp::CompareI
        };
        self.node(Op::ScalarIntrinsic(op), IrType::Int, vec![a, b])
    }
}

#[cfg(test)]
mod tests;
