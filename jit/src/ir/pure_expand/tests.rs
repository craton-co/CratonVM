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
