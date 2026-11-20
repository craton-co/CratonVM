// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Lifting an emitted PTX line into a structured [`Instr`].
//!
//! # The one rule
//!
//! [`Instr::lift`] returns a structured instruction **only if rendering it
//! back as PTX reproduces the input line byte for byte**. Anything the
//! grammar below does not cover, or covers but would spell differently, stays
//! [`Instr::Raw`] and is refused by every non-PTX renderer.
//!
//! That is what makes lifting safe to do at the single place lines are born
//! (`Body`'s `fmt::Write`) instead of at ~190 emission sites: it cannot change
//! the PTX, because a line that would change is not lifted, and
//! `tests/golden/ptx/` pins the rest. It also means the lifter can be wrong
//! only in the direction of refusing a kernel, never of mistranslating one —
//! what a lifted instruction MEANS on another target is the renderer's job
//! and is tested there.

use super::{
    render_ptx_instr, BinOp, Cmp, CvtMode, FOp, Imm, Instr, Operand, ParamRef, PredOp, PredUse,
    SpecialReg, Ty, UnOp,
};

impl Instr {
    /// Lift one rendered line (including its trailing newline), or keep it
    /// as [`Instr::Raw`]. See the module docs for the guarantee.
    pub fn lift(line: String) -> Instr {
        match parse_line(&line) {
            Some(i) => {
                let mut again = String::with_capacity(line.len());
                render_ptx_instr(&i, &mut again);
                if again == line {
                    i
                } else {
                    Instr::Raw(line)
                }
            }
            None => Instr::Raw(line),
        }
    }
}

impl Ty {
    /// The inverse of [`Ty::ptx`].
    pub fn from_ptx(s: &str) -> Option<Ty> {
        Some(match s {
            "s8" => Ty::S8,
            "u8" => Ty::U8,
            "s16" => Ty::S16,
            "u16" => Ty::U16,
            "s32" => Ty::S32,
            "u32" => Ty::U32,
            "s64" => Ty::S64,
            "u64" => Ty::U64,
            "f16" => Ty::F16,
            "f32" => Ty::F32,
            "f64" => Ty::F64,
            "b16" => Ty::B16,
            "b32" => Ty::B32,
            "b64" => Ty::B64,
            _ => return None,
        })
    }
}

fn parse_line(line: &str) -> Option<Instr> {
    let body = line.strip_suffix('\n')?;
    if let Some(name) = body.strip_suffix(':') {
        return is_ident(name).then(|| Instr::Label(name.to_string()));
    }
    let t = body.strip_prefix("    ")?.strip_suffix(';')?;
    if t.starts_with(' ') {
        return None;
    }
    if let Some(rest) = t.strip_prefix('@') {
        let (p, inner) = rest.split_once(' ')?;
        let (negated, reg) = match p.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, p),
        };
        if !is_pred_reg(reg) {
            return None;
        }
        let pred = PredUse {
            reg: reg.to_string(),
            negated,
        };
        if let Some(target) = inner.strip_prefix("bra ") {
            return is_ident(target).then(|| Instr::Bra {
                target: target.to_string(),
                pred: Some(pred),
            });
        }
        let inner = parse_op(inner)?;
        // A guarded branch is `Bra`, handled above; a guard around a label,
        // a return or another guard has no meaning to render.
        if matches!(
            inner,
            Instr::Label(_) | Instr::Bra { .. } | Instr::Ret | Instr::Guarded { .. }
        ) {
            return None;
        }
        return Some(Instr::Guarded {
            pred,
            inner: Box::new(inner),
        });
    }
    parse_op(t)
}

fn parse_op(t: &str) -> Option<Instr> {
    let (mn, ops) = t.split_once(' ').unwrap_or((t, ""));
    let ops: Vec<&str> = if ops.is_empty() {
        Vec::new()
    } else {
        ops.split(", ").collect()
    };
    let parts: Vec<&str> = mn.split('.').collect();
    match (parts.as_slice(), ops.as_slice()) {
        (["ret"], []) => Some(Instr::Ret),
        (["bra"], [target]) if is_ident(target) => Some(Instr::Bra {
            target: (*target).to_string(),
            pred: None,
        }),
        (["mov", ty], [dst, src]) => parse_mov(Ty::from_ptx(ty)?, dst, src),
        (["ld", "param", ty], [dst, mem]) => Some(Instr::LdParam {
            dst: reg(dst)?,
            ty: Ty::from_ptx(ty)?,
            param: param_ref(bracketed(mem)?)?,
        }),
        (["ld", "global", ty], [dst, mem]) => Some(Instr::LdGlobal {
            dst: reg(dst)?,
            ty: Ty::from_ptx(ty)?,
            addr: reg(bracketed(mem)?)?,
        }),
        (["st", "global", ty], [mem, value]) => Some(Instr::StGlobal {
            ty: Ty::from_ptx(ty)?,
            addr: reg(bracketed(mem)?)?,
            value: operand(value)?,
        }),
        (["red", "global", "add", ty], [mem, value]) => Some(Instr::RedAdd {
            ty: Ty::from_ptx(ty)?,
            addr: reg(bracketed(mem)?)?,
            value: reg(value)?,
        }),
        (["mad", "lo", ty], [d, a, b, c]) => Some(Instr::MadLo {
            ty: Ty::from_ptx(ty)?,
            dst: reg(d)?,
            a: reg(a)?,
            b: reg(b)?,
            c: reg(c)?,
        }),
        (["mad", "wide", "s32"], [d, index, size, base]) => Some(Instr::ElementAddr {
            dst: reg(d)?,
            base: reg(base)?,
            index: reg(index)?,
            elem_size: size.parse().ok()?,
            param: None,
        }),
        (["setp", cmp, ty], [d, a, b]) => Some(Instr::Setp {
            cmp: Cmp::from_ptx(cmp)?,
            ty: Ty::from_ptx(ty)?,
            dst: reg(d)?,
            a: operand(a)?,
            b: operand(b)?,
            and: None,
        }),
        (["setp", cmp, "and", ty], [d, a, b, p]) => {
            let negated = p.starts_with('!');
            let predicate = p.strip_prefix('!').unwrap_or(p);
            if !is_pred_reg(predicate) {
                return None;
            }
            Some(Instr::Setp {
                cmp: Cmp::from_ptx(cmp)?,
                ty: Ty::from_ptx(ty)?,
                dst: reg(d)?,
                a: operand(a)?,
                b: operand(b)?,
                and: Some(PredUse {
                    reg: predicate.to_string(),
                    negated,
                }),
            })
        }
        (["selp", ty], [d, a, b, p]) => Some(Instr::Selp {
            ty: Ty::from_ptx(ty)?,
            dst: reg(d)?,
            a: operand(a)?,
            b: operand(b)?,
            pred: reg(p)?,
        }),
        ([op @ ("and" | "or"), "pred"], [d, a, b]) => Some(Instr::PredBin {
            op: if *op == "and" {
                PredOp::And
            } else {
                PredOp::Or
            },
            dst: reg(d)?,
            a: reg(a)?,
            b: reg(b)?,
        }),
        ([op @ ("add" | "sub" | "mul" | "div"), "rn", ty], [d, a, b]) => Some(Instr::FArith {
            op: FOp::from_ptx(op)?,
            ty: float_ty(ty)?,
            dst: reg(d)?,
            a: operand(a)?,
            b: operand(b)?,
        }),
        ([op @ ("div" | "rem"), ty], [d, a, b]) => {
            let ty = Ty::from_ptx(ty)?;
            matches!(ty, Ty::S32 | Ty::U32 | Ty::S64 | Ty::U64).then_some(())?;
            Some(Instr::IDivRem {
                rem: *op == "rem",
                ty,
                dst: reg(d)?,
                a: operand(a)?,
                b: operand(b)?,
            })
        }
        (["mul", "lo", ty], [d, a, b]) => bin(BinOp::Mul, ty, d, a, b),
        ([op @ ("add" | "sub" | "and" | "or" | "xor" | "shl" | "shr"), ty], [d, a, b]) => {
            let op = match *op {
                "add" => BinOp::Add,
                "sub" => BinOp::Sub,
                "and" => BinOp::And,
                "or" => BinOp::Or,
                "xor" => BinOp::Xor,
                "shl" => BinOp::Shl,
                _ => BinOp::Shr,
            };
            bin(op, ty, d, a, b)
        }
        ([op @ ("neg" | "abs"), ty], [d, s]) => Some(Instr::Unary {
            op: if *op == "neg" { UnOp::Neg } else { UnOp::Abs },
            ty: Ty::from_ptx(ty)?,
            dst: reg(d)?,
            src: operand(s)?,
        }),
        (["sqrt", "rn", ty], [d, s]) => Some(Instr::Unary {
            op: UnOp::Sqrt,
            ty: float_ty(ty)?,
            dst: reg(d)?,
            src: operand(s)?,
        }),
        (["ex2", "approx", "f32"], [d, s]) => Some(Instr::Unary {
            op: UnOp::Ex2Approx,
            ty: Ty::F32,
            dst: reg(d)?,
            src: operand(s)?,
        }),
        (["fma", "rn", ty], [d, a, b, c]) => Some(Instr::Fma {
            ty: float_ty(ty)?,
            dst: reg(d)?,
            a: operand(a)?,
            b: operand(b)?,
            c: operand(c)?,
        }),
        (["cvt", rest @ ..], [d, s]) => {
            let (mode, tys) = match rest {
                ["rn", tys @ ..] => (CvtMode::Rn, tys),
                ["rzi", tys @ ..] => (CvtMode::Rzi, tys),
                tys => (CvtMode::Exact, tys),
            };
            let [dty, sty] = tys else { return None };
            Some(Instr::Cvt {
                mode,
                dst_ty: Ty::from_ptx(dty)?,
                src_ty: Ty::from_ptx(sty)?,
                dst: reg(d)?,
                src: reg(s)?,
            })
        }
        _ => None,
    }
}

fn parse_mov(ty: Ty, dst: &str, src: &str) -> Option<Instr> {
    let dst = reg(dst)?;
    if let Some(which) = SpecialReg::from_ptx(src) {
        return (ty == Ty::U32).then_some(Instr::ReadSpecial { dst, which });
    }
    if src.starts_with('%') {
        let src = reg(src)?;
        return Some(match ty {
            Ty::B32 => Instr::Bitcast {
                dst,
                src,
                width: 32,
            },
            Ty::B64 => Instr::Bitcast {
                dst,
                src,
                width: 64,
            },
            _ => Instr::Mov { dst, ty, src },
        });
    }
    match operand(src)? {
        Operand::Imm(imm) => Some(Instr::MovImm { dst, ty, imm }),
        Operand::Reg(_) => None,
    }
}

fn bin(op: BinOp, ty: &str, d: &str, a: &str, b: &str) -> Option<Instr> {
    Some(Instr::Bin {
        op,
        ty: Ty::from_ptx(ty)?,
        dst: reg(d)?,
        a: operand(a)?,
        b: operand(b)?,
    })
}

fn float_ty(s: &str) -> Option<Ty> {
    let t = Ty::from_ptx(s)?;
    matches!(t, Ty::F32 | Ty::F64).then_some(t)
}

fn bracketed(s: &str) -> Option<&str> {
    s.strip_prefix('[')?.strip_suffix(']')
}

/// A register name: `%` then letters and digits.
fn reg(s: &str) -> Option<String> {
    let body = s.strip_prefix('%')?;
    (!body.is_empty() && body.chars().all(|c| c.is_ascii_alphanumeric())).then(|| s.to_string())
}

fn is_pred_reg(s: &str) -> bool {
    s.strip_prefix("%p")
        .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn operand(s: &str) -> Option<Operand> {
    if s.starts_with('%') {
        return reg(s).map(Operand::Reg);
    }
    imm(s).map(Operand::Imm)
}

fn imm(s: &str) -> Option<Imm> {
    let hex = |d: &str, n: usize| d.len() == n && d.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(d) = s.strip_prefix("0f") {
        return hex(d, 8).then(|| Imm::F32(s.to_string()));
    }
    if let Some(d) = s.strip_prefix("0d") {
        return hex(d, 16).then(|| Imm::F64(s.to_string()));
    }
    if let Some(d) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return (!d.is_empty() && d.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| Imm::Hex(s.to_string()));
    }
    s.parse::<i64>().ok().map(Imm::Int)
}

fn param_ref(name: &str) -> Option<ParamRef> {
    match name {
        "failure_flag" => return Some(ParamRef::FailureFlag),
        "tid_base" => return Some(ParamRef::TidBase),
        "ret_ptr" => return Some(ParamRef::RetPtr),
        "ret_len" => return Some(ParamRef::RetLen),
        _ => {}
    }
    let rest = name.strip_prefix('p')?;
    let (digits, suffix) = match rest.find('_') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let n: u32 = digits.parse().ok()?;
    match suffix {
        "_ptr" => Some(ParamRef::ArrayPtr(n)),
        "_len" => Some(ParamRef::ArrayLen(n)),
        "" => Some(ParamRef::Scalar(n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
