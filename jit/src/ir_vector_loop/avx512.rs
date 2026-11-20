// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wide independent expressions and strictly ordered floating-point reductions.
use super::*;

pub(super) fn enabled() -> bool {
    crate::x64::has_avx512f()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_AVX512")
        && true}
pub(super) fn ordered_enabled() -> bool {
    enabled()
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
mod ordered_float_tests;
