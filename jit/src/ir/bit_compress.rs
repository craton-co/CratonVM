// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT j27 wave 6, lane T1BCE: `Integer.compress(II)I`, `Long.compress(JJ)J`,
//! `Integer.expand(II)I` and `Long.expand(JJ)J` as [`ScalarOp`] rows lowered
//! to one BMI2 instruction each, as HotSpot 25's C2 does (`_compress_i` /
//! `_compress_l` / `_expand_i` / `_expand_l`, `PEXT` / `PDEP` when the CPU
//! has BMI2). Without the switch, or on a host without BMI2, nothing here
//! answers and the site stays the JDK body (a loop of shifts and masks).
//!
//! # Why it is exact
//!
//! The JDK specifies `compress(i, mask)` as the bits of `i` at the set
//! positions of `mask`, packed into the low end in order, the rest zero; and
//! `expand(i, mask)` as the low bits of `i`, in order, deposited at the set
//! positions of `mask`, the rest zero. Those are, word for word, Intel's
//! definitions of `PEXT` (parallel extract) and `PDEP` (parallel deposit)
//! with the value in the VEX `vvvv` operand and the mask in `r/m` (SDM
//! vol. 2B, `PEXT` / `PDEP`). The 32-bit forms zero the upper half of the
//! destination, which is this tier's `int` register convention.
//!
//! # Encoding
//!
//! `PEXT r, r, r/m` is `VEX.LZ.F3.0F38.W0/W1 F5 /r`, `PDEP` the same with
//! `F2`. The lowering always emits `op rAX, rAX, rCX`: value in RAX (the
//! scalar arm's operand 0 load), mask in RCX. Three-byte VEX (`C4`):
//!
//! * byte 1 `R̄ X̄ B̄ m-mmmm` = `1 1 1 00010` = `E2` (no extended register,
//!   map `0F38`);
//! * byte 2 `W v̄v̄v̄v̄ L pp` = `W 1111 0 pp`: `vvvv` = RAX (0) inverted,
//!   `L = 0` (`LZ`), `pp` = `10` for `F3` (PEXT) / `11` for `F2` (PDEP);
//! * opcode `F5`; ModRM `11 000 001` = `C1` (reg = RAX, r/m = RCX).
//!
//! # Switch
//!
//! `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND`, default ON since 2026-10-28 (j27
//! WARNING flip; =0 restores; was opt-in: unmeasured, and `PEXT` / `PDEP`
//! are microcoded and slow on AMD before Zen 3, so a host A/B decides), under `CRATONVM_JIT_IR_SCALAR_INTRINSICS`. Read per call,
//! no `static`.

use super::ScalarOp;

/// `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND` (j27 wave 6, lane T1BCE; default
/// ON since 2026-10-28 (j27 WARNING flip; =0 restores)): the four rows of
/// this module.
pub(crate) fn ir_bit_compress_expand_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND")
}

/// The family `class.method descriptor` names, when the switch is on and
/// the host has BMI2. `try_ir_scalar_intrinsic` asks it (after its own
/// `CRATONVM_JIT_IR_SCALAR_INTRINSICS` gate).
pub(super) fn bit_compress_row(class: &str, method: &str, descriptor: &str) -> Option<ScalarOp> {
    let sop = match (class, method, descriptor) {
        ("java/lang/Integer", "compress", "(II)I") => ScalarOp::CompressI,
        ("java/lang/Long", "compress", "(JJ)J") => ScalarOp::CompressL,
        ("java/lang/Integer", "expand", "(II)I") => ScalarOp::ExpandI,
        ("java/lang/Long", "expand", "(JJ)J") => ScalarOp::ExpandL,
        _ => return None,
    };
    (ir_bit_compress_expand_enabled() && crate::x64::has_bmi2()).then_some(sop)
}

/// The five bytes of `PEXT` / `PDEP rAX, rAX, rCX` for `sop` (module notes);
/// `None` for every other family.
pub(crate) fn pext_pdep_rax_rax_rcx(sop: ScalarOp) -> Option<[u8; 5]> {
    let (pp, w): (u8, u8) = match sop {
        ScalarOp::CompressI => (0b10, 0),
        ScalarOp::CompressL => (0b10, 1),
        ScalarOp::ExpandI => (0b11, 0),
        ScalarOp::ExpandL => (0b11, 1),
        _ => return None,
    };
    // W | inverted vvvv (RAX = 0000 -> 1111) | L = 0 | pp.
    let byte2 = (w << 7) | (0b1111 << 3) | pp;
    Some([0xC4, 0xE2, byte2, 0xF5, 0xC1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Graph, IrType, Op};

    const SW: &str = "CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND";

    fn with_switch<T>(v: Option<&str>, f: impl FnOnce() -> T) -> T {
        cratonvm_types::flags::with_thread_overrides(&[(SW, v)], f)
    }

    /// The JDK's specification of `compress`, bit by bit over `width` bits.
    fn ref_compress(x: u64, mask: u64, width: u32) -> u64 {
        let mut r = 0u64;
        let mut k = 0;
        for b in 0..width {
            if mask >> b & 1 == 1 {
                r |= (x >> b & 1) << k;
                k += 1;
            }
        }
        r
    }

    /// The JDK's specification of `expand`, bit by bit over `width` bits.
    fn ref_expand(x: u64, mask: u64, width: u32) -> u64 {
        let mut r = 0u64;
        let mut k = 0;
        for b in 0..width {
            if mask >> b & 1 == 1 {
                r |= (x >> k & 1) << b;
                k += 1;
            }
        }
        r
    }

    #[test]
    fn the_encodings_are_the_sdm_bytes() {
        // pext eax, eax, ecx / pext rax, rax, rcx / pdep eax, eax, ecx /
        // pdep rax, rax, rcx.
        let enc = pext_pdep_rax_rax_rcx;
        assert_eq!(enc(ScalarOp::CompressI), Some([0xC4, 0xE2, 0x7A, 0xF5, 0xC1]));
        assert_eq!(enc(ScalarOp::CompressL), Some([0xC4, 0xE2, 0xFA, 0xF5, 0xC1]));
        assert_eq!(enc(ScalarOp::ExpandI), Some([0xC4, 0xE2, 0x7B, 0xF5, 0xC1]));
        assert_eq!(enc(ScalarOp::ExpandL), Some([0xC4, 0xE2, 0xFB, 0xF5, 0xC1]));
        assert_eq!(enc(ScalarOp::RotateLeftI), None);
        assert_eq!(enc(ScalarOp::BitCountL), None);
    }

    #[test]
    fn the_encodings_decode_field_by_field() {
        for (sop, wide, deposit) in [
            (ScalarOp::CompressI, false, false),
            (ScalarOp::CompressL, true, false),
            (ScalarOp::ExpandI, false, true),
            (ScalarOp::ExpandL, true, true),
        ] {
            let b = pext_pdep_rax_rax_rcx(sop).expect("a row");
            assert_eq!(b[0], 0xC4, "{sop:?}: three-byte VEX");
            assert_eq!(b[1] & 0xE0, 0xE0, "{sop:?}: R X B clear (inverted set)");
            assert_eq!(b[1] & 0x1F, 0b00010, "{sop:?}: map 0F38");
            assert_eq!(b[2] >> 7 == 1, wide, "{sop:?}: W");
            assert_eq!((b[2] >> 3) & 0xF, 0xF, "{sop:?}: vvvv = RAX, inverted");
            assert_eq!((b[2] >> 2) & 1, 0, "{sop:?}: L = 0");
            assert_eq!(b[2] & 3, if deposit { 0b11 } else { 0b10 }, "{sop:?}: pp F2 / F3");
            assert_eq!(b[3], 0xF5, "{sop:?}: opcode");
            assert_eq!(b[4], 0b11_000_001, "{sop:?}: reg RAX, r/m RCX");
        }
    }

    #[test]
    fn the_rows_need_the_switch_and_bmi2_and_their_exact_triples() {
        let row = bit_compress_row;
        with_switch(Some("0"), || {
            assert_eq!(row("java/lang/Integer", "compress", "(II)I"), None, "=0 is off");
            assert_eq!(row("java/lang/Long", "expand", "(JJ)J"), None, "=0 is off");
        });
        with_switch(None, || {
            let want = crate::x64::has_bmi2().then_some(ScalarOp::CompressI);
            assert_eq!(row("java/lang/Integer", "compress", "(II)I"), want, "default ON");
        });
        with_switch(Some("1"), || {
            let host = crate::x64::has_bmi2();
            let want = |s: ScalarOp| host.then_some(s);
            assert_eq!(row("java/lang/Integer", "compress", "(II)I"), want(ScalarOp::CompressI));
            assert_eq!(row("java/lang/Long", "compress", "(JJ)J"), want(ScalarOp::CompressL));
            assert_eq!(row("java/lang/Integer", "expand", "(II)I"), want(ScalarOp::ExpandI));
            assert_eq!(row("java/lang/Long", "expand", "(JJ)J"), want(ScalarOp::ExpandL));
            assert_eq!(row("java/lang/Integer", "compress", "(JJ)J"), None);
            assert_eq!(row("java/lang/Long", "expand", "(II)I"), None);
            assert_eq!(row("java/lang/Short", "compress", "(SS)S"), None);
            // The public door agrees (under its own scalar-intrinsics gate).
            if crate::ir::ir_scalar_intrinsics_enabled() {
                let door = crate::ir::try_ir_scalar_intrinsic("java/lang/Long", "expand", "(JJ)J");
                assert_eq!(door, want(ScalarOp::ExpandL));
            }
        });
    }

    #[test]
    fn the_rows_type_their_operands_as_the_descriptors() {
        for (sop, long) in [
            (ScalarOp::CompressI, false),
            (ScalarOp::CompressL, true),
            (ScalarOp::ExpandI, false),
            (ScalarOp::ExpandL, true),
        ] {
            let ty = if long { IrType::Long } else { IrType::Int };
            assert_eq!(sop.arity(), 2, "{sop:?}");
            assert_eq!((sop.input_ty(0), sop.input_ty(1)), (ty, ty), "{sop:?} operands");
            assert_eq!(sop.result_type(), ty, "{sop:?} result");
            assert_eq!(sop.operands_are_long(), long, "{sop:?} width");
            assert!(!sop.is_fp() && sop.raw_bits_move().is_none(), "{sop:?}: a GP family");
        }
    }

    #[test]
    fn the_reference_meets_the_jdk_identities() {
        // The tests' own reference, checked against the identities the JDK
        // javadoc states, so a wrong reference cannot pass a wrong lowering.
        assert_eq!(ref_compress(0xCAFE_BABE, 0xFF00, 32), 0xBA);
        assert_eq!(ref_expand(0xBA, 0xFF00, 32), 0xBA00);
        assert_eq!(ref_compress(0b1011, 0b1010, 32), 0b11);
        assert_eq!(ref_expand(0b11, 0b1010, 32), 0b1010);
        for &(x, m) in &[(0x1234_5678_u64, 0x0F0F_0F0F_u64), (u64::MAX, 0x8000_0001), (0x5, 0)] {
            assert_eq!(ref_expand(ref_compress(x, m, 64), m, 64), x & m);
            let n = m.count_ones();
            let low = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
            assert_eq!(ref_compress(ref_expand(x, m, 64), m, 64), x & low);
        }
    }

    const INT_EDGES: [u32; 12] = [
        0,
        0xFFFF_FFFF,
        0x5555_5555,
        0xAAAA_AAAA,
        1,
        0x8000_0000,
        0x0000_8000,
        0x8000_0001,
        0x0F0F_0F0F,
        0x1234_5678,
        0xCAFE_BABE,
        0x7FFF_FFFF,
    ];
    const LONG_EDGES: [u64; 12] = [
        0,
        u64::MAX,
        0x5555_5555_5555_5555,
        0xAAAA_AAAA_AAAA_AAAA,
        1,
        0x8000_0000_0000_0000,
        0x0000_0001_0000_0000,
        0x8000_0000_0000_0001,
        0x0F0F_0F0F_0F0F_0F0F,
        0x0123_4567_89AB_CDEF,
        0xFFFF_FFFF_0000_0000,
        0x7FFF_FFFF_FFFF_FFFF,
    ];

    /// `f(a, b) = sop(a, b)` as a graph, scheduled and lowered.
    #[cfg(target_arch = "x86_64")]
    fn lowered(sop: ScalarOp) -> crate::CompiledMethod {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), sop.input_ty(0), vec![start], None);
        let b = g.add(Op::Param(1), sop.input_ty(1), vec![start], None);
        let v = g.add(Op::ScalarIntrinsic(sop), sop.result_type(), vec![a, b], Some(0));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, v], Some(1));
        let schedule = crate::ir_schedule::schedule(&g);
        let helpers = crate::JitRuntimeHelpers::default();
        crate::ir_lower::lower(&g, &schedule, 2, 2, &helpers).expect("the row lowers on BMI2")
    }

    /// The real lowering, executed, on every pair of edge values: what a
    /// compiled caller computes. Skipped on a host without BMI2 (the bytes
    /// would #UD; the rows are not recognised there).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn lowered_rows_compute_the_jdk_answers_on_edge_masks() {
        if !std::is_x86_feature_detected!("bmi2") {
            return;
        }
        let run = |cm: &crate::CompiledMethod, x: i64, m: i64| {
            // SAFETY: a straight-line two-argument body this test lowered
            // with an all-zero helper table; it calls nothing, and the host
            // has BMI2 (checked above).
            unsafe { cm.try_call(&[x, m]) }.expect("call")
        };
        for (sop, f) in [
            (ScalarOp::CompressI, ref_compress as fn(u64, u64, u32) -> u64),
            (ScalarOp::ExpandI, ref_expand),
        ] {
            let cm = lowered(sop);
            for x in INT_EDGES {
                for m in INT_EDGES {
                    // Cast: an `int` argument travels sign-extended; the
                    // result is the low 32 bits.
                    let got = run(&cm, i64::from(x as i32), i64::from(m as i32)) as u32;
                    let want = f(u64::from(x), u64::from(m), 32) as u32;
                    assert_eq!(got, want, "{sop:?}({x:#x}, {m:#x})");
                }
            }
        }
        for (sop, f) in [
            (ScalarOp::CompressL, ref_compress as fn(u64, u64, u32) -> u64),
            (ScalarOp::ExpandL, ref_expand),
        ] {
            let cm = lowered(sop);
            for x in LONG_EDGES {
                for m in LONG_EDGES {
                    let got = run(&cm, x as i64, m as i64) as u64;
                    assert_eq!(got, f(x, m, 64), "{sop:?}({x:#x}, {m:#x})");
                }
            }
        }
    }
}
