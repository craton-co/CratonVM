use super::*;
use crate::ir::IrBuilder;

fn jmin(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.to_bits() == (-0.0f64).to_bits() {
        return b;
    }
    if a <= b {
        a
    } else {
        b
    }
}

fn jmax(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.to_bits() == (-0.0f64).to_bits() {
        return b;
    }
    if a >= b {
        a
    } else {
        b
    }
}

fn jminf(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.to_bits() == (-0.0f32).to_bits() {
        return b;
    }
    if a <= b {
        a
    } else {
        b
    }
}

fn jmaxf(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.to_bits() == (-0.0f32).to_bits() {
        return b;
    }
    if a >= b {
        a
    } else {
        b
    }
}

/// The doubles the min/max tests cross: both zeros, infinities, extremes,
/// a subnormal, and two distinguishable NaNs (one with its sign bit set,
/// the case HotSpot's own AVX sequence answers differently) plus a
/// signalling NaN, whose quiet bit must not be set by a selection.
fn doubles() -> Vec<f64> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        2.5,
        -2.5,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::MIN,
        5e-324,
        f64::from_bits(0x7FF8_0000_0000_0001),
        f64::from_bits(0xFFF8_0000_0000_0BAD),
        f64::from_bits(0x7FF0_0000_0000_0042),
    ]
}

fn floats() -> Vec<f32> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        3.25,
        -3.25,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::MIN_POSITIVE,
        f32::MAX,
        f32::MIN,
        1e-45,
        f32::from_bits(0x7FC0_0001),
        f32::from_bits(0xFFC0_0BAD),
        f32::from_bits(0x7F80_0042),
    ]
}

/// `extern "C" fn(a_bits, b_bits) -> result_bits` around the sequence, with
/// `a` in XMM`ra` and `b` in XMM`rb` (both caller-saved on Win64 and SysV:
/// XMM2-XMM5 only).
fn avx_minmax_fn(dbl: bool, is_min: bool, ra: u8, rb: u8) -> crate::ExecutableBuffer {
    assert!((2..=5).contains(&ra) && (2..=5).contains(&rb));
    let mut body: Vec<u8> = Vec::new();
    for (xmm, gpr) in [(ra, CALL_ARG_REGS[0]), (rb, CALL_ARG_REGS[1])] {
        assert!(gpr < 8);
        // MOVQ xmm, r64 (66 REX.W 0F 6E /r) / MOVD xmm, r32 (66 0F 6E /r).
        body.push(0x66);
        if dbl {
            body.push(0x48);
        }
        body.extend_from_slice(&[0x0F, 0x6E, 0xC0 | (xmm << 3) | gpr]);
    }
    body.extend_from_slice(&fp_minmax_avx_bytes(dbl, is_min, ra, rb));
    // MOVQ RAX, XMM0 / MOVD EAX, XMM0; RET.
    if dbl {
        body.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]);
    } else {
        body.extend_from_slice(&[0x66, 0x0F, 0x7E, 0xC0]);
    }
    body.push(0xC3);
    let mut buf = crate::ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

/// IROPS19-5: the sequence, EXECUTED for every pair of interesting
/// operands at both widths, in distinct registers and in one register
/// (`Math.min(x, x)`), against the JDK bodies bit for bit.
#[test]
fn the_branch_free_minmax_matches_the_jdk_bodies_bit_for_bit() {
    if !crate::x64::has_avx() {
        return;
    }
    for is_min in [true, false] {
        let name = if is_min { "min" } else { "max" };
        for (ra, rb) in [(2u8, 3u8), (5, 4)] {
            let buf = avx_minmax_fn(true, is_min, ra, rb);
            // SAFETY: `buf` is finalized and holds a complete
            // `extern "C" fn(u64, u64) -> u64` writing RAX and XMM0-XMM5,
            // all caller-saved on both ABIs.
            let f = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
            };
            for &a in &doubles() {
                for &b in &doubles() {
                    let want = if is_min { jmin(a, b) } else { jmax(a, b) };
                    assert_eq!(
                        f(a.to_bits(), b.to_bits()),
                        want.to_bits(),
                        "{name}({a:?}, {b:?}) in xmm{ra}/xmm{rb}"
                    );
                }
            }
            let buf = avx_minmax_fn(false, is_min, ra, rb);
            // SAFETY: as above; the float bits ride the low 32 bits.
            let f = unsafe {
                std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
            };
            for &a in &floats() {
                for &b in &floats() {
                    let want = if is_min { jminf(a, b) } else { jmaxf(a, b) };
                    let got = f(u64::from(a.to_bits()), u64::from(b.to_bits()));
                    // Cast: the float result is the low 32 bits.
                    assert_eq!(
                        got as u32,
                        want.to_bits(),
                        "{name}({a:?}f, {b:?}f) in xmm{ra}/xmm{rb}"
                    );
                }
            }
        }
        // One register for both operands.
        let buf = avx_minmax_fn(true, is_min, 3, 3);
        // SAFETY: as above.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
        };
        for &a in &doubles() {
            assert_eq!(
                f(a.to_bits(), a.to_bits()),
                a.to_bits(),
                "{name}({a:?}, same)"
            );
        }
    }
}

/// The VEX fields for registers past XMM7 (`R̄`/`B̄` clear, the high bit of
/// the inverted `vvvv`), checked against hand-assembled encodings.
#[test]
fn the_vex_encodings_reach_every_xmm_register() {
    // VMINSD xmm0, xmm3, xmm2 / VMINSD xmm0, xmm10, xmm9.
    assert_eq!(
        &fp_minmax_avx_bytes(true, true, 2, 3)[..5],
        &[0xC4, 0xE1, 0x63, 0x5D, 0xC2]
    );
    assert_eq!(
        &fp_minmax_avx_bytes(true, true, 9, 10)[..5],
        &[0xC4, 0xC1, 0x2B, 0x5D, 0xC1]
    );
    // VMAXSS xmm1, xmm9, xmm10: the second instruction of max(a=9, b=10).
    assert_eq!(
        &fp_minmax_avx_bytes(false, false, 9, 10)[5..10],
        &[0xC4, 0xC1, 0x32, 0x5F, 0xCA]
    );
    // VBLENDVPD xmm0, xmm0, xmm11, xmm1: map 0F3A, pp 66, is4 = 0x10.
    let b = fp_minmax_avx_bytes(true, true, 11, 4);
    assert_eq!(&b[b.len() - 6..], &[0xC4, 0xC3, 0x79, 0x4B, 0xC3, 0x10]);
}

/// `static double f(double x, double y, double c, int n) { double acc =
/// c; for (int i = 0; i < n; i++) acc = x * y + acc; return acc; }`
const FMA_LOOP: [u8; 31] = [
    0x18, 0x04, // 0:  dload 4
    0x39, 0x07, // 2:  dstore 7
    0x03, // 4:  iconst_0
    0x36, 0x09, // 5:  istore 9
    0x15, 0x09, // 7:  iload 9       <- header
    0x15, 0x06, // 9:  iload 6
    0xa2, 0x00, 0x11, // 11: if_icmpge +17 -> 28
    0x26, // 14: dload_0
    0x28, // 15: dload_2
    0x6b, // 16: dmul
    0x18, 0x07, // 17: dload 7
    0x63, // 19: dadd
    0x39, 0x07, // 20: dstore 7
    0x84, 0x09, 0x01, // 22: iinc 9, 1
    0xa7, 0xff, 0xee, // 25: goto -18 -> 7
    0x18, 0x07, // 28: dload 7
    0xaf, // 30: dreturn
];

/// Build [`FMA_LOOP`] and rewrite its `dadd(dmul(x, y), acc)` to `sop`:
/// `Math.fma(x, y, acc)`, or `Math.min/max(acc, x)`. `None` when this
/// host's builder declines the method.
fn loop_with(sop: ScalarOp) -> Option<Graph> {
    let mut b = IrBuilder::new(4, 10);
    b.set_param_types(&[IrType::Double, IrType::Double, IrType::Double, IrType::Int]);
    let mut g = b.build(&FMA_LOOP, FMA_LOOP.len())?;
    let add = (0..g.nodes.len())
        .find(|&i| g.nodes[i].op == Op::Add && g.nodes[i].ty == IrType::Double)?;
    let (mul, acc) = (g.nodes[add].inputs[0], g.nodes[add].inputs[1]);
    if g.nodes[mul as usize].op != Op::Mul {
        return None;
    }
    let (x, y) = (
        g.nodes[mul as usize].inputs[0],
        g.nodes[mul as usize].inputs[1],
    );
    let inputs = if sop.arity() == 3 {
        vec![x, y, acc]
    } else {
        vec![acc, x]
    };
    g.nodes[add].op = Op::ScalarIntrinsic(sop);
    // Cast: node index -> NodeId.
    g.set_inputs(add as NodeId, inputs);
    crate::ir_optimize::optimize(&mut g);
    Some(g)
}

fn lower_graph(g: &Graph) -> CompiledMethod {
    let schedule = crate::ir_schedule::schedule(g);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; with no safepoint flag the body emits no poll and calls no helper.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    lower_inner(
        g,
        &schedule,
        4,
        10,
        &helpers,
        &HashMap::new(),
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &crate::ir::IrInlineFrameSites::default(),
    )
    .expect("the loop lowers")
}

fn call(cm: &CompiledMethod, x: f64, y: f64, c: f64, n: i64) -> u64 {
    // Cast: FP arguments ride the all-GPR entry ABI as raw bits.
    let args = [
        x.to_bits() as i64,
        y.to_bits() as i64,
        c.to_bits() as i64,
        n,
    ];
    // SAFETY: a `double f(double, double, double, int)` body lowered by
    // this module; it calls no helper.
    let got = unsafe { cm.try_call(&args) }.expect("call");
    // Cast: the double result's raw bits.
    got as u64
}

/// Does `code` hold `VFMADD213SD xmm0, xmm1, <reg>` (register form)?
fn has_fma_register_form(code: &[u8]) -> bool {
    code.windows(5).any(|w| {
        w[0] == 0xC4
            && (w[1] == 0xE2 || w[1] == 0xC2)
            && w[2] == 0xF1
            && w[3] == 0xA9
            && w[4] >= 0xC0
    })
}

/// IROPS2-8: under the linear-scan file the loop-carried addend of
/// `Math.fma` is RESIDENT, so the lowering takes its register-operand
/// form -- which the straight-line harness never reaches -- and answers
/// exactly as the home-word form does, a fused result included.
#[test]
fn a_resident_fma_addend_is_read_in_place_and_answers_alike() {
    if !crate::x64::inline_fma_enabled() {
        return;
    }
    let Some(g) = loop_with(ScalarOp::FmaD) else {
        return; // this host's builder declines the method
    };
    let resident = {
        let _ls = LsForce::on();
        lower_graph(&g)
    };
    let home = {
        let _ls = LsForce::off();
        lower_graph(&g)
    };
    assert!(
        has_fma_register_form(resident.code_bytes()),
        "the addend is loop-carried, so the file keeps it in a register"
    );
    assert!(
        !has_fma_register_form(home.code_bytes()),
        "no file, no register form"
    );
    let eps = f64::EPSILON;
    // `(1+e)(1-e) - 1` fused is exactly `-e^2` (0xB970...); unfused it is 0.
    assert_eq!(
        call(&resident, 1.0 + eps, 1.0 - eps, -1.0, 1),
        0xB970_0000_0000_0000
    );
    for &(x, y, c) in &[
        (0.1, 10.0, -1.0),
        (1.5, -2.25, 0.125),
        (f64::MAX, 2.0, -f64::MAX),
        (3.0, 7.0, f64::NAN),
        (-0.0, 0.0, -0.0),
    ] {
        for n in [0i64, 1, 2, 5, 100] {
            assert_eq!(
                call(&resident, x, y, c, n),
                call(&home, x, y, c, n),
                "fma loop x={x:?} y={y:?} c={c:?} n={n}"
            );
        }
    }
}

