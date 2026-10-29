// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 8, lane W8INTR (`docs/known-issues/jit/j27-intrinsic-coverage-vs-hotspot-20261028.md`):
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
//! unmeasured, the "intrinsic loses to bytecode" rule. In `--jdk-only` the
//! site is the JDK body (two ALU ops, normally spliced), so the gain is
//! `--compatible`'s funnel call per op.

use super::{Emit, PureExpansion};
use super::super::{IrType, NodeId, Op, ScalarOp};

/// `CRATONVM_JIT_IR_INT_COMPARE_EXPAND` (j27 wave 8, lane W8INTR; default
/// OFF, `=1` on): the four rows of this module.
pub fn ir_int_compare_expand_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_INT_COMPARE_EXPAND")
}

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
mod tests {
    use super::super::{expand, try_ir_pure_expansion};
    use super::*;
    use super::super::super::{Graph, IrBuilder};
    use std::collections::HashMap;

    const ROWS: [PureExpansion; 4] = [
        PureExpansion::CompareUnsignedI,
        PureExpansion::CompareUnsignedL,
        PureExpansion::SignumI,
        PureExpansion::SignumL,
    ];

    /// The Java value of data node `id` (`int` sign-extended).
    fn eval(g: &Graph, params: &[i64], id: NodeId) -> i64 {
        let n = &g.nodes[id as usize];
        let x = |k: usize| eval(g, params, n.inputs[k]);
        let raw = match n.op {
            Op::Const(c) => c,
            Op::Param(i) => params[i as usize],
            Op::Xor => x(0) ^ x(1),
            Op::ScalarIntrinsic(ScalarOp::CompareI) => {
                // Casts: the `int` operands' Java width.
                i64::from((x(0) as i32).cmp(&(x(1) as i32)) as i8)
            }
            Op::ScalarIntrinsic(ScalarOp::CompareL) => i64::from(x(0).cmp(&x(1)) as i8),
            ref op => panic!("unexpected {op:?} at n{id}"),
        };
        if n.ty == IrType::Int {
            // Cast: the node's Java width.
            i64::from(raw as i32)
        } else {
            raw
        }
    }

    fn expanded(expansion: PureExpansion) -> (Graph, NodeId) {
        let mut g = Graph::empty(0);
        let ty = operand_type(expansion);
        let args: Vec<NodeId> = (0..expansion.arity())
            .map(|i| g.add(Op::Param(i as u16), ty, vec![], None))
            .collect();
        let r = expand(&mut g, expansion, &args, 7);
        (g, r)
    }

    const INTS: [i32; 10] = [
        0,
        1,
        -1,
        2,
        -2,
        i32::MIN,
        i32::MAX,
        i32::MIN + 1,
        0x7FFF_0000,
        0x8000_FFFF_u32 as i32,
    ];
    const LONGS: [i64; 10] = [
        0,
        1,
        -1,
        2,
        -2,
        i64::MIN,
        i64::MAX,
        i64::MIN + 1,
        0x0000_0001_0000_0000,
        0x8000_0000_FFFF_FFFF_u64 as i64,
    ];

    /// JDK 25 `Integer.compareUnsigned` / `Long.compareUnsigned`, written as
    /// the javadoc defines it (an unsigned order), not as the expansion is.
    fn ref_cmpu32(a: i32, b: i32) -> i64 {
        // Casts: reinterpret as unsigned, the javadoc's definition.
        i64::from((a as u32).cmp(&(b as u32)) as i8)
    }

    fn ref_cmpu64(a: i64, b: i64) -> i64 {
        i64::from((a as u64).cmp(&(b as u64)) as i8)
    }

    /// JDK 25 `Integer.signum` / `Long.signum` bodies, literally.
    fn ref_signum32(i: i32) -> i64 {
        i64::from((i >> 31) | ((i.wrapping_neg() as u32) >> 31) as i32)
    }

    fn ref_signum64(i: i64) -> i64 {
        // Cast: the body's `(int)` of a value in {-1, 0, 1}.
        i64::from(((i >> 63) | ((i.wrapping_neg() as u64) >> 63) as i64) as i32)
    }

    #[test]
    fn j27_w8intr_expansions_answer_the_jdk_bodies_on_edge_inputs() {
        let (g, r) = expanded(PureExpansion::CompareUnsignedI);
        for a in INTS {
            for b in INTS {
                let got = eval(&g, &[i64::from(a), i64::from(b)], r);
                assert_eq!(got, ref_cmpu32(a, b), "compareUnsigned({a:#x}, {b:#x})");
            }
        }
        let (g, r) = expanded(PureExpansion::CompareUnsignedL);
        for a in LONGS {
            for b in LONGS {
                let got = eval(&g, &[a, b], r);
                assert_eq!(got, ref_cmpu64(a, b), "compareUnsigned({a:#x}L, {b:#x}L)");
            }
        }
        let (g, r) = expanded(PureExpansion::SignumI);
        for v in INTS {
            assert_eq!(eval(&g, &[i64::from(v)], r), ref_signum32(v), "signum({v:#x})");
        }
        let (g, r) = expanded(PureExpansion::SignumL);
        for v in LONGS {
            assert_eq!(eval(&g, &[v], r), ref_signum64(v), "signum({v:#x}L)");
        }
        // The literal answers the probe also prints.
        assert_eq!(ref_cmpu32(-1, 0), 1);
        assert_eq!(ref_cmpu32(i32::MIN, i32::MAX), 1);
        assert_eq!(ref_signum32(i32::MIN), -1);
        assert_eq!(ref_signum64(i64::MIN), -1);
    }

    #[test]
    fn j27_w8intr_each_row_builds_its_anchor_once_as_the_int_result() {
        for e in ROWS {
            let (g, r) = expanded(e);
            let anchor = Op::ScalarIntrinsic(e.anchor());
            let anchors: Vec<usize> =
                (0..g.nodes.len()).filter(|&i| g.nodes[i].op == anchor).collect();
            assert_eq!(anchors, vec![r as usize], "{e:?}: the result is the one anchor");
            assert_eq!(g.nodes[r as usize].ty, IrType::Int, "{e:?}: an int result");
            assert_eq!(e.anchor(), compare_op(e), "{e:?}");
        }
    }

    #[test]
    fn j27_w8intr_rows_need_the_switch_and_their_exact_triples() {
        let on = |c, m, d| try_ir_pure_expansion(c, m, d);
        let key = "CRATONVM_JIT_IR_INT_COMPARE_EXPAND";
        cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            assert_eq!(on("java/lang/Integer", "compareUnsigned", "(II)I"), None, "default OFF");
        });
        cratonvm_types::flags::with_thread_overrides(&[(key, Some("1"))], || {
            if !super::super::super::ir_scalar_intrinsics_enabled() {
                assert_eq!(on("java/lang/Integer", "compareUnsigned", "(II)I"), None);
                return;
            }
            let rows = [
                ("java/lang/Integer", "compareUnsigned", "(II)I"),
                ("java/lang/Long", "compareUnsigned", "(JJ)I"),
                ("java/lang/Integer", "signum", "(I)I"),
                ("java/lang/Long", "signum", "(J)I"),
            ];
            for ((c, m, d), want) in rows.into_iter().zip(ROWS) {
                assert_eq!(on(c, m, d), Some(want), "{c}.{m}{d}");
            }
            // Not these bodies: `Integer.compare` is the scalar row,
            // `Long.compareUnsigned(II)` no method, `Short.compareUnsigned` a
            // subtraction, `divideUnsigned` a throwing divide.
            assert_eq!(on("java/lang/Integer", "compare", "(II)I"), None);
            assert_eq!(on("java/lang/Long", "compareUnsigned", "(II)I"), None);
            assert_eq!(on("java/lang/Short", "compareUnsigned", "(SS)I"), None);
            assert_eq!(on("java/lang/Integer", "divideUnsigned", "(II)I"), None);
        });
    }

    /// `static int f(T a[, T b]) { return C.m(a[, b]); }` built, optionally
    /// optimized, scheduled and LOWERED.
    #[cfg(target_arch = "x86_64")]
    fn compiled(expansion: PureExpansion, optimize: bool) -> crate::CompiledMethod {
        let wide = operand_type(expansion) == IrType::Long;
        let (load, slot) = if wide { (0x1e_u8, 2usize) } else { (0x1a_u8, 1usize) };
        let n = expansion.arity();
        // Cast: `?load_0 + slot`, a one-byte opcode.
        let mut code: Vec<u8> = (0..n).map(|i| load + (i * slot) as u8).collect();
        let site = code.len();
        code.extend_from_slice(&[0xb8, 0x00, 0x01, 0xac]);
        let len = code.len();
        code.extend_from_slice(&[0, 0]);
        let mut builder = IrBuilder::new(n, n * slot);
        builder.set_param_types(&vec![operand_type(expansion); n]);
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

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn j27_w8intr_lowered_rows_compute_the_jdk_answers() {
        for optimize in [false, true] {
            let call = |cm: &crate::CompiledMethod, args: &[i64]| {
                // SAFETY: a straight-line `f(int|long[, int|long])` body this
                // test lowered with an all-zero helper table; it calls nothing.
                let raw = unsafe { cm.try_call(args) }.expect("call");
                // Cast: this tier's `int` result is the low 32 bits.
                i64::from(raw as i32)
            };
            let cm = compiled(PureExpansion::CompareUnsignedI, optimize);
            for a in INTS {
                for b in INTS {
                    let got = call(&cm, &[i64::from(a), i64::from(b)]);
                    let want = ref_cmpu32(a, b);
                    assert_eq!(got, want, "compareUnsigned({a:#x}, {b:#x}) optimize={optimize}");
                }
            }
            let cm = compiled(PureExpansion::CompareUnsignedL, optimize);
            for a in LONGS {
                for b in LONGS {
                    let got = call(&cm, &[a, b]);
                    let want = ref_cmpu64(a, b);
                    assert_eq!(got, want, "compareUnsigned({a:#x}L, {b:#x}L) opt={optimize}");
                }
            }
            let cm = compiled(PureExpansion::SignumI, optimize);
            for v in INTS {
                let got = call(&cm, &[i64::from(v)]);
                assert_eq!(got, ref_signum32(v), "signum({v:#x}) optimize={optimize}");
            }
            let cm = compiled(PureExpansion::SignumL, optimize);
            for v in LONGS {
                let got = call(&cm, &[v]);
                assert_eq!(got, ref_signum64(v), "signum({v:#x}L) optimize={optimize}");
            }
        }
    }
}
