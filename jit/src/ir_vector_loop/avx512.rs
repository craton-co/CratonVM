// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wide independent expressions and strictly ordered floating-point reductions.
use super::*;

pub(super) fn enabled() -> bool {
    crate::x64::has_avx512f()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_AVX512")
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_AVX512_VECTOR_LOOP")
}
pub(super) fn ordered_enabled() -> bool {
    enabled() && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_VECTOR_ORDERED_FP")
}
fn tree(e: &VExpr, kind: VecKind) -> bool {
    match e {
        VExpr::Load(_) | VExpr::Inv(_) => true,
        VExpr::Bin(op, l, r) => {
            let ok = match kind {
                VecKind::Double | VecKind::Float => {
                    matches!(op, VOp::Add | VOp::Sub | VOp::Mul | VOp::Div)
                }
                VecKind::Int => !matches!(op, VOp::Div),
                VecKind::Long => matches!(op, VOp::Add | VOp::Sub | VOp::And | VOp::Or | VOp::Xor),
                _ => false,
            };
            ok && tree(l, kind) && tree(r, kind)
        }
        _ => false,
    }
}
pub(super) fn eligible(p: &VecLoopPlan) -> bool {
    enabled()
        && matches!(
            p.kind,
            VecKind::Double | VecKind::Float | VecKind::Int | VecKind::Long
        )
        && p.scan.is_none()
        && p.widen.is_none()
        && p.scoped.is_none()
        && !p.iota
        && p.shift_counts.is_empty()
        && p.base_kinds
            .iter()
            .all(|&k| mem_bytes(k) == p.kind.elem_bytes())
        && (p.reduce.is_none() || (p.ordered_fp() && ordered_enabled()))
        && tree(&p.value, p.kind)
}
// Low sixteen vector registers only. Memory always uses disp32: EVEX disp8
// is scaled by the tuple size and cannot encode arbitrary VM header offsets.
fn prefix(o: &mut Vec<u8>, reg: u8, rm: u8, map: u8, w: bool, v: u8, pp: u8) {
    debug_assert!(reg < 16 && rm < 16 && v < 16);
    o.extend([
        0x62,
        (if reg < 8 { 0x80 } else { 0 }) | 0x40 | (if rm < 8 { 0x20 } else { 0 }) | 0x10 | map,
        (u8::from(w) << 7) | ((!v & 15) << 3) | 4 | pp,
        0x48,
    ]);
}
fn rr(o: &mut Vec<u8>, map: u8, w: bool, pp: u8, op: u8, d: u8, a: u8, b: u8) {
    prefix(o, d, b, map, w, a, pp);
    o.extend([op, 0xC0 | ((d & 7) << 3) | (b & 7)]);
}
fn memory(o: &mut Vec<u8>, reg: u8, base: u8, width: u8, disp: i8) {
    let scale = if width == 8 { 3 } else { 2 };
    o.extend([
        0x84 | ((reg & 7) << 3),
        (scale << 6) | (IV_REG << 3) | (base & 7),
    ]);
    o.extend((disp as i32).to_le_bytes());
}
fn load(o: &mut Vec<u8>, k: VecKind, d: u8, base: u8, disp: i8) {
    let (w, pp, op) = match k {
        VecKind::Double => (true, 1, 0x10),
        VecKind::Float => (false, 0, 0x10),
        _ => (true, 2, 0x6F),
    };
    prefix(o, d, base, 1, w, 0, pp);
    o.push(op);
    memory(o, d, base, k.elem_bytes(), disp);
}
fn store(o: &mut Vec<u8>, k: VecKind, s: u8, base: u8, disp: i8) {
    let (w, pp, op) = match k {
        VecKind::Double => (true, 1, 0x11),
        VecKind::Float => (false, 0, 0x11),
        _ => (true, 2, 0x7F),
    };
    prefix(o, s, base, 1, w, 0, pp);
    o.push(op);
    memory(o, s, base, k.elem_bytes(), disp);
}
pub(super) fn broadcast(o: &mut Vec<u8>, k: VecKind, d: u8, s: u8) {
    let (w, op) = match k {
        VecKind::Double => (true, 0x19),
        VecKind::Float => (false, 0x18),
        VecKind::Long => (true, 0x59),
        _ => (false, 0x58),
    };
    rr(o, 2, w, 1, op, d, 0, s);
}
pub(super) fn scalar_move(d: u8, s: u8) -> Vec<u8> {
    let mut o = Vec::new();
    let rex = 0x40 | ((d >> 3) << 2) | (s >> 3);
    if rex != 0x40 {
        o.push(rex);
    }
    o.extend([0x0F, 0x28, 0xC0 | ((d & 7) << 3) | (s & 7)]);
    o
}
fn expr(
    p: &VecLoopPlan,
    r: &StepRegs<'_>,
    e: &VExpr,
    d: u8,
    free: &mut Vec<u8>,
    disp: i8,
    o: &mut Vec<u8>,
) -> Option<()> {
    match e {
        VExpr::Load(b) => load(o, p.kind, d, BASE_REGS[*b], disp),
        VExpr::Inv(i) => rr(o, 1, true, 2, 0x6F, d, 0, *r.inv_regs.get(*i)?),
        VExpr::Bin(op, l, right) => {
            expr(p, r, l, d, free, disp, o)?;
            let (map, pp, code) = op_code(p.kind, *op);
            let w = matches!(p.kind, VecKind::Double | VecKind::Long);
            match right.as_ref() {
                VExpr::Load(b) => {
                    let base = BASE_REGS[*b];
                    prefix(o, d, base, map, w, d, pp);
                    o.push(code);
                    memory(o, d, base, p.kind.elem_bytes(), disp);
                }
                VExpr::Inv(i) => rr(o, map, w, pp, code, d, d, *r.inv_regs.get(*i)?),
                _ => {
                    let t = free.pop()?;
                    expr(p, r, right, t, free, disp, o)?;
                    rr(o, map, w, pp, code, d, d, t);
                    free.push(t);
                }
            }
        }
        _ => return None,
    }
    Some(())
}
/// A fill's last `R11` elements (`1 <= R11 < lanes`) at `j` (RDX) in one
/// store masked by `K1 = (1 << R11) - 1`: lanes past the bound are neither
/// written nor faulted. `None` unless the plan stores one invariant into its
/// one base (RAX), the only shape whose lanes read nothing past the bound.
/// Clobbers RCX, R9, K1 and the flags.
pub(super) fn masked_fill_tail(p: &VecLoopPlan, r: &StepRegs<'_>, disp: i8) -> Option<Vec<u8>> {
    let VExpr::Inv(i) = p.value else {
        return None;
    };
    if p.reduce.is_some() || p.bases.len() != 1 || !eligible(p) {
        return None;
    }
    let s = *r.inv_regs.get(i)?;
    // W0 for int / float lanes: the mask then counts dwords.
    let (w, pp, op) = match p.kind {
        VecKind::Double => (true, 1, 0x11),
        VecKind::Float => (false, 0, 0x11),
        VecKind::Long => (true, 2, 0x7F),
        VecKind::Int => (false, 2, 0x7F),
        _ => return None,
    };
    let mut o = vec![
        0x44, 0x89, 0xD9, // MOV ECX, R11D
        0x41, 0xB9, 1, 0, 0, 0, // MOV R9D, 1
        0x41, 0xD3, 0xE1, // SHL R9D, CL
        0x41, 0xFF, 0xC9, // DEC R9D
        0xC4, 0xC1, 0x78, 0x92, 0xC9, // KMOVW K1, R9D
    ];
    let at = o.len();
    prefix(&mut o, s, BASE_REGS[0], 1, w, 0, pp);
    o[at + 3] |= 1; // EVEX.aaa = K1
    o.push(op);
    memory(&mut o, s, BASE_REGS[0], p.kind.elem_bytes(), disp);
    Some(o)
}

pub(super) fn step(p: &VecLoopPlan, r: &StepRegs<'_>, disp: i8) -> Option<Vec<u8>> {
    let mut o = Vec::new();
    let mut free: Vec<u8> = r.temps.iter().rev().copied().collect();
    let d = free.pop()?;
    expr(p, r, &p.value, d, &mut free, disp, &mut o)?;
    if let Some(red) = p.reduce {
        let acc = r.acc?;
        // Save independent products, then consume every lane in source order.
        // No reassociation, horizontal sum, FMA, call or poll inside this span.
        o.extend([0x48, 0x83, 0xEC, 64]); // SUB RSP,64
        prefix(&mut o, d, 4, 1, true, 0, 2);
        o.extend([0x7F, 0x04 | ((d & 7) << 3), 0x24]);
        let pp = if p.kind == VecKind::Double { 3 } else { 2 };
        let op = if red.op == RedOp::Sub { 0x5C } else { 0x58 };
        for lane in 0..(64 / p.kind.elem_bytes()) {
            vex3(&mut o, acc, 0, 4, 1, false, acc, false, pp, op);
            o.extend([0x44 | ((acc & 7) << 3), 0x24, lane * p.kind.elem_bytes()]);
        }
        o.extend([0x48, 0x83, 0xC4, 64]); // ADD RSP,64
    } else {
        store(&mut o, p.kind, d, BASE_REGS[p.store_base], disp);
    }
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{disp0, fake_array, plan_of, run_on_host};
    use super::*;
    fn wide(f: impl FnOnce()) {
        if !crate::x64::has_avx512f() {
            return;
        }
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT", None),
                ("CRATONVM_AVX512", Some("1")),
                ("CRATONVM_JIT_AVX512_VECTOR_LOOP", Some("1")),
                ("CRATONVM_JIT_IR_VECTOR_ORDERED_FP", Some("1")),
            ],
            f,
        );
    }
    #[test]
    fn high_register_encodings_match_gnu_assembler() {
        let mut o = Vec::new();
        rr(&mut o, 1, true, 1, 0x58, 8, 9, 10);
        assert_eq!(o, vec![0x62, 0x51, 0xB5, 0x48, 0x58, 0xC2]);
        o.clear();
        load(&mut o, VecKind::Double, 9, 9, 16);
        assert_eq!(
            o,
            vec![0x62, 0x51, 0xFD, 0x48, 0x10, 0x8C, 0xD1, 16, 0, 0, 0]
        );
        o.clear();
        broadcast(&mut o, VecKind::Double, 9, 0);
        assert_eq!(o, vec![0x62, 0x72, 0xFD, 0x48, 0x19, 0xC8]);
    }
    #[test]
    fn wide_int_product_preserves_wrapping_and_guard_bytes() {
        wide(|| {
            let a: Vec<i32> = (0..16).map(|i| i32::MAX - i * 777).collect();
            let b: Vec<i32> = (0..16).map(|i| 3 + i * 11111).collect();
            let aa = fake_array(
                4,
                &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
            );
            let bb = fake_array(
                4,
                &b.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
            );
            let mut cc = fake_array(4, &[0u8; 64]);
            let mut p = plan_of(
                VecKind::Int,
                VExpr::Bin(VOp::Mul, Box::new(VExpr::Load(0)), Box::new(VExpr::Load(1))),
                3,
                0,
            );
            p.store_base = 2;
            let r = StepRegs {
                temps: &[0, 1],
                inv_regs: &[],
                acc: None,
                nan: None,
            };
            assert!(p.uses_avx512());
            assert_eq!(p.lanes(), 16);
            assert_eq!(p.unroll(), 2);
            let code = step(&p, &r, disp0()).unwrap();
            let mut args = [
                aa.as_ptr() as u64,
                bb.as_ptr() as u64,
                cc.as_mut_ptr() as u64,
                0,
                0,
                0,
                0,
                0,
            ];
            run_on_host(&code, &mut args).unwrap();
            let h = cratonvm_types::HEADER_SIZE;
            for i in 0..16 {
                assert_eq!(
                    i32::from_le_bytes(cc[h + i * 4..h + i * 4 + 4].try_into().unwrap()),
                    a[i].wrapping_mul(b[i])
                );
            }
            assert!(cc[h + 64..].iter().all(|&b| b == 0xA5));
        });
    }
    /// The masked tail writes exactly elements `j .. j + n` of a fill, for
    /// every `n` below one step, and nothing past them.
    #[test]
    fn masked_fill_tail_writes_exactly_n_lanes() {
        wide(|| {
            for (kind, elem) in [(VecKind::Int, 4usize), (VecKind::Long, 8)] {
                let lanes = 64 / elem;
                for n in 1..lanes {
                    for j in [0usize, 1] {
                        let mut arr = fake_array(elem, &vec![0u8; 64 + elem]);
                        let p = plan_of(kind, VExpr::Inv(0), 1, 1);
                        let r = StepRegs { temps: &[0], inv_regs: &[2], acc: None, nan: None };
                        // ZMM2 = -1 in every lane; R11 = n, the arg block kept.
                        let mut code = vec![0x49, 0xC7, 0xC2, 0xFF, 0xFF, 0xFF, 0xFF]; // MOV R10, -1
                        code.extend([0x66, 0x49, 0x0F, 0x6E, 0xC2]); // MOVQ XMM0, R10
                        broadcast(&mut code, kind, 2, 0);
                        code.extend([0x41, 0x53, 0x49, 0xC7, 0xC3]); // PUSH R11; MOV R11, imm32
                        code.extend((n as u32).to_le_bytes());
                        code.extend(masked_fill_tail(&p, &r, disp0()).unwrap());
                        code.push(0x41);
                        code.push(0x5B); // POP R11
                        let mut args = [arr.as_mut_ptr() as u64, 0, 0, 0, j as u64, 0, 0, 0];
                        run_on_host(&code, &mut args).unwrap();
                        let h = cratonvm_types::HEADER_SIZE;
                        for (k, &b) in arr[h..h + 64 + elem].iter().enumerate() {
                            let written = (j * elem..(j + n) * elem).contains(&k);
                            assert_eq!(b, if written { 0xFF } else { 0 }, "{kind:?} n={n} j={j} byte {k}");
                        }
                        assert!(arr[h + 64 + elem..].iter().all(|&b| b == 0xA5));
                    }
                }
            }
        });
    }
    #[test]
    fn ordered_double_reduction_keeps_cancellation_and_seed() {
        wide(|| {
            for op in [RedOp::Add, RedOp::Sub] {
                for seed in [17.0f64, -0.0] {
                    let a: Vec<f64> = (0..16).map(|i| [1e20, 1., -1e20, 1.][i % 4]).collect();
                    let b = vec![1.0f64; 16];
                    let aa = fake_array(
                        8,
                        &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
                    );
                    let bb = fake_array(
                        8,
                        &b.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
                    );
                    let mut p = plan_of(
                        VecKind::Double,
                        VExpr::Bin(VOp::Mul, Box::new(VExpr::Load(0)), Box::new(VExpr::Load(1))),
                        2,
                        0,
                    );
                    p.reduce = Some(VecReduce { acc: 0, op });
                    let r = StepRegs {
                        temps: &[0, 1],
                        inv_regs: &[],
                        acc: Some(2),
                        nan: None,
                    };
                    let mut code = vec![0x49, 0xBA];
                    code.extend(seed.to_bits().to_le_bytes());
                    code.extend([0x66, 0x49, 0x0F, 0x6E, 0xD2]);
                    code.extend(step(&p, &r, disp0()).unwrap());
                    code.extend(step(&p, &r, disp0() + 64).unwrap());
                    code.extend(scalar_move(0, 2));
                    code.extend([0x66, 0x48, 0x0F, 0x7E, 0xC0]);
                    let mut args = [aa.as_ptr() as u64, bb.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
                    run_on_host(&code, &mut args).unwrap();
                    let want = a.iter().zip(&b).fold(seed, |s, (&a, &b)| {
                        if op == RedOp::Sub {
                            s - a * b
                        } else {
                            s + a * b
                        }
                    });
                    assert_eq!(args[5], want.to_bits());
                }
            }
        });
    }
}

#[cfg(test)]
mod ordered_float_tests {
    use super::super::tests::{disp0, fake_array, plan_of, run_on_host};
    use super::*;
    #[test]
    fn ordered_float_and_double_preserve_seed_and_cancellation() {
        if !crate::x64::has_avx512f() || !crate::x64::has_avx2() {
            return;
        }
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT", None),
                ("CRATONVM_AVX512", Some("1")),
                ("CRATONVM_JIT_AVX512_VECTOR_LOOP", Some("1")),
                ("CRATONVM_JIT_IR_VECTOR_ORDERED_FP", Some("1")),
            ],
            || {
                for kind in [VecKind::Float, VecKind::Double] {
                    for pattern in 0..3 {
                        for op in [RedOp::Add, RedOp::Sub] {
                            let width = kind.elem_bytes() as usize;
                            let n = 128 / width;
                            let values: Vec<f64> = (0..n)
                                .map(|i| match pattern {
                                    0 => [1e20, 1., -1e20, 1.][i % 4],
                                    1 => 0.,
                                    _ => 1.,
                                })
                                .collect();
                            let input: Vec<u8> = values
                                .iter()
                                .flat_map(|&v| {
                                    if width == 8 {
                                        v.to_le_bytes().to_vec()
                                    } else {
                                        (v as f32).to_le_bytes().to_vec()
                                    }
                                })
                                .collect();
                            let arr = fake_array(width, &input);
                            for seed in [-0.0f64, 17.0] {
                                let mut p = plan_of(kind, VExpr::Load(0), 1, 0);
                                p.reduce = Some(VecReduce { acc: 0, op });
                                let r = StepRegs {
                                    temps: &[0, 1],
                                    inv_regs: &[],
                                    acc: Some(2),
                                    nan: None,
                                };
                                let bits = if width == 8 {
                                    seed.to_bits()
                                } else {
                                    (seed as f32).to_bits() as u64
                                };
                                let mut code = vec![0x49, 0xBA];
                                code.extend(bits.to_le_bytes());
                                code.extend([0x66, 0x49, 0x0F, 0x6E, 0xD2]);
                                code.extend(step(&p, &r, disp0()).unwrap());
                                code.extend(step(&p, &r, disp0() + 64).unwrap());
                                code.extend(scalar_move(0, 2));
                                if width == 8 {
                                    code.extend([0x66, 0x48, 0x0F, 0x7E, 0xC0]);
                                } else {
                                    code.extend([0x66, 0x0F, 0x7E, 0xC0]);
                                }
                                let mut args = [arr.as_ptr() as u64, 0, 0, 0, 0, 0, 0, 0];
                                run_on_host(&code, &mut args).unwrap();
                                let want = if width == 8 {
                                    values
                                        .iter()
                                        .fold(
                                            seed,
                                            |s, &v| if op == RedOp::Sub { s - v } else { s + v },
                                        )
                                        .to_bits()
                                } else {
                                    values
                                        .iter()
                                        .fold(seed as f32, |s, &v| {
                                            if op == RedOp::Sub {
                                                s - v as f32
                                            } else {
                                                s + v as f32
                                            }
                                        })
                                        .to_bits() as u64
                                };
                                assert_eq!(
                                    args[5], want,
                                    "{kind:?} pattern={pattern} op={op:?} seed={seed}"
                                );
                            }
                        }
                    }
                }
            },
        );
    }
}
