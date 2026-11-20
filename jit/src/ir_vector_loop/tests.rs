use super::*;
use super::avx2_step_code as step_code;

pub(super) fn plan_of(kind: VecKind, value: VExpr, bases: usize, invs: usize) -> VecLoopPlan {
    VecLoopPlan {
        header: 0,
        entry_pred: 0,
        header_bci: None,
        loop_blocks: Vec::new(),
        iv: 0,
        bound: 0,
        bound_length_of: None,
        kind,
        bases: (10..10 + bases as NodeId).collect(),
        base_kinds: vec![
            match kind {
                VecKind::Double => MemKind::Double,
                VecKind::Float => MemKind::Float,
                VecKind::Int => MemKind::Int,
                VecKind::Long => MemKind::Long,
                VecKind::Byte => MemKind::Byte,
                VecKind::Short => MemKind::Short,
            };
            bases
        ],
        invariants: (20..20 + invs as NodeId).collect(),
        shift_counts: Vec::new(),
        store_base: 0,
        value,
        reduce: None,
        scan: None,
        widen: None,
        iota: false,
        scoped: None,
    }
}

#[test]
fn the_encodings_match_the_sdm() {
    let mut o = Vec::new();
    // vmovupd ymm0, [rax + rdx*8 + 0x10]
    emit_load(&mut o, VecKind::Double, 0, 0, 0x10);
    assert_eq!(o, [0xC4, 0xE1, 0x7D, 0x10, 0x44, 0xD0, 0x10]);
    o.clear();
    // vmovupd [rcx + rdx*8 + 0x30], ymm9
    emit_store(&mut o, VecKind::Double, 9, 1, 0x30);
    assert_eq!(o, [0xC4, 0x61, 0x7D, 0x11, 0x4C, 0xD1, 0x30]);
    o.clear();
    // vmulpd ymm0, ymm0, [r8 + rdx*8 + 0x10]
    emit_binop(
        &mut o,
        VecKind::Double,
        VOp::Mul,
        0,
        0,
        Src::Mem {
            base: 8,
            disp: 0x10,
        },
    );
    assert_eq!(o, [0xC4, 0xC1, 0x7D, 0x59, 0x44, 0xD0, 0x10]);
    o.clear();
    // vaddpd ymm1, ymm1, ymm15
    emit_binop(&mut o, VecKind::Double, VOp::Add, 1, 1, Src::Reg(15));
    assert_eq!(o, [0xC4, 0xC1, 0x75, 0x58, 0xCF]);
    o.clear();
    // vbroadcastsd ymm15, xmm0
    emit_broadcast(&mut o, VecKind::Double, 15, 0);
    assert_eq!(o, [0xC4, 0x62, 0x7D, 0x19, 0xF8]);
    o.clear();
    // vpmulld ymm0, ymm0, [rax + rdx*4 + 0x10]
    emit_binop(
        &mut o,
        VecKind::Int,
        VOp::Mul,
        0,
        0,
        Src::Mem {
            base: 0,
            disp: 0x10,
        },
    );
    assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x40, 0x44, 0x90, 0x10]);
    o.clear();
    // vmovdqu ymm0, [rax + rdx*4 + 0x10]
    emit_load(&mut o, VecKind::Int, 0, 0, 0x10);
    assert_eq!(o, [0xC4, 0xE1, 0x7E, 0x6F, 0x44, 0x90, 0x10]);
    o.clear();
    // vmovaps ymm0, ymm15
    emit_move(&mut o, 0, 15);
    assert_eq!(o, [0xC4, 0xC1, 0x7C, 0x28, 0xC7]);
}

/// The round-21 encodings (VEC21-1/-3/-4), each against the SDM form.
#[test]
fn the_r21_encodings_match_the_sdm() {
    let mut o = Vec::new();
    // vpxor ymm0, ymm0, [rcx + rdx*4 + 0x10]   VEX.256.66.0F EF /r
    emit_binop(
        &mut o,
        VecKind::Int,
        VOp::Xor,
        0,
        0,
        Src::Mem {
            base: 1,
            disp: 0x10,
        },
    );
    assert_eq!(o, [0xC4, 0xE1, 0x7D, 0xEF, 0x44, 0x91, 0x10]);
    o.clear();
    // vpand ymm1, ymm1, [rax + rdx*8 + 0x30]   (long lanes, scale 8)
    emit_binop(
        &mut o,
        VecKind::Long,
        VOp::And,
        1,
        1,
        Src::Mem {
            base: 0,
            disp: 0x30,
        },
    );
    assert_eq!(o, [0xC4, 0xE1, 0x75, 0xDB, 0x4C, 0xD0, 0x30]);
    o.clear();
    // vpor ymm0, ymm0, ymm9
    emit_binop(&mut o, VecKind::Int, VOp::Or, 0, 0, Src::Reg(9));
    assert_eq!(o, [0xC4, 0xC1, 0x7D, 0xEB, 0xC1]);
    o.clear();
    // vpslld ymm0, ymm0, 3   VEX.256.66.0F 72 /6 ib
    emit_shift(&mut o, VecKind::Int, VShift::Shl, 0, 0, 3).unwrap();
    assert_eq!(o, [0xC4, 0xE1, 0x7D, 0x72, 0xF0, 0x03]);
    o.clear();
    // vpsrad ymm1, ymm1, 31   72 /4
    emit_shift(&mut o, VecKind::Int, VShift::Sar, 1, 1, 31).unwrap();
    assert_eq!(o, [0xC4, 0xE1, 0x75, 0x72, 0xE1, 0x1F]);
    o.clear();
    // vpsrlq ymm9, ymm9, 7   73 /2 ib, vvvv = ymm9, rm = ymm9 (VEX.B)
    emit_shift(&mut o, VecKind::Long, VShift::Shr, 9, 9, 7).unwrap();
    assert_eq!(o, [0xC4, 0xC1, 0x35, 0x73, 0xD1, 0x07]);
    o.clear();
    // No VPSRAQ in AVX2.
    assert!(emit_shift(&mut o, VecKind::Long, VShift::Sar, 0, 0, 1).is_none());
    // vpaddb ymm0, ymm0, [rax + rdx*1 + 0x10]   66 0F FC, scale 1
    emit_binop(
        &mut o,
        VecKind::Byte,
        VOp::Add,
        0,
        0,
        Src::Mem {
            base: 0,
            disp: 0x10,
        },
    );
    assert_eq!(o, [0xC4, 0xE1, 0x7D, 0xFC, 0x44, 0x10, 0x10]);
    o.clear();
    // vpmullw ymm0, ymm0, [rcx + rdx*2 + 0x10]   66 0F D5, scale 2
    emit_binop(
        &mut o,
        VecKind::Short,
        VOp::Mul,
        0,
        0,
        Src::Mem {
            base: 1,
            disp: 0x10,
        },
    );
    assert_eq!(o, [0xC4, 0xE1, 0x7D, 0xD5, 0x44, 0x51, 0x10]);
    o.clear();
    // vmovdqu ymm0, [rax + rdx*1 + 0x10]
    emit_load(&mut o, VecKind::Byte, 0, 0, 0x10);
    assert_eq!(o, [0xC4, 0xE1, 0x7E, 0x6F, 0x44, 0x10, 0x10]);
    o.clear();
    // vpbroadcastb ymm1, xmm0   VEX.256.66.0F38.W0 78 /r
    emit_broadcast(&mut o, VecKind::Byte, 1, 0);
    assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x78, 0xC8]);
    o.clear();
    // vpbroadcastw ymm1, xmm0   79 /r
    emit_broadcast(&mut o, VecKind::Short, 1, 0);
    assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x79, 0xC8]);
    o.clear();
    // vpxor ymm1, ymm1, ymm1
    emit_zero(&mut o, 1);
    assert_eq!(o, [0xC4, 0xE1, 0x75, 0xEF, 0xC9]);
    o.clear();
    // The int fold of ymm1 through xmm0.
    let f = fold_code(VecKind::Int, 1, 0).unwrap();
    assert_eq!(
        f,
        [
            0xC4, 0xE3, 0x7D, 0x39, 0xC8, 0x01, // vextracti128 xmm0, ymm1, 1
            0xC4, 0xE1, 0x71, 0xFE, 0xC8, // vpaddd xmm1, xmm1, xmm0
            0xC4, 0xE1, 0x79, 0x70, 0xC1, 0x4E, // vpshufd xmm0, xmm1, 0x4e
            0xC4, 0xE1, 0x71, 0xFE, 0xC8, // vpaddd xmm1, xmm1, xmm0
            0xC4, 0xE1, 0x79, 0x70, 0xC1, 0xB1, // vpshufd xmm0, xmm1, 0xb1
            0xC4, 0xE1, 0x71, 0xFE, 0xC8, // vpaddd xmm1, xmm1, xmm0
            0xC4, 0xE1, 0x79, 0x7E, 0xC8, // vmovd eax, xmm1
        ]
    );
    // The long fold ends in VMOVQ (W1) after one shuffle.
    let f = fold_code(VecKind::Long, 1, 0).unwrap();
    assert_eq!(&f[f.len() - 5..], [0xC4, 0xE1, 0xF9, 0x7E, 0xC8]);
    assert!(fold_code(VecKind::Double, 1, 0).is_none());
    assert!(fold_code(VecKind::Int, 1, 1).is_none());
}

// ---- INTRINSICS22-3 (tier2 wave 2, page tag `intr22`) -----------------
//
// The tests above compare emitted bytes with literals derived by hand;
// where the literal and the emitter share a misreading of the SDM they
// agree on a wrong byte. The tests below DECODE what the emitters produce
// with an independent x86 decoder (`iced-x86`, a dev-dependency at the
// version and features `vm` uses) and compare the mnemonic and every
// operand: registers (swept over 0..15, so every VEX/REX extension bit is
// hit), the memory base / index / scale / displacement, and immediates. The
// expected mnemonics come from the SDM instruction names the emitters'
// doc comments cite, not from the bytes.

use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic as Mn, OpKind, Register};

/// Every instruction of `bytes`; the decoder must consume them exactly.
fn intr22_decode(bytes: &[u8], what: &str) -> Vec<Instruction> {
    let mut decoder = Decoder::new(64, bytes, DecoderOptions::NONE);
    let decoded: Vec<Instruction> = decoder.iter().collect();
    let mut total = 0usize;
    for i in &decoded {
        assert!(
            !i.is_invalid(),
            "{what}: an invalid instruction in {bytes:02X?}"
        );
        total += i.len();
    }
    assert_eq!(
        total,
        bytes.len(),
        "{what}: the decoder consumed {total} of {} bytes of {bytes:02X?}",
        bytes.len()
    );
    decoded
}

fn intr22_one(bytes: &[u8], what: &str) -> Instruction {
    let decoded = intr22_decode(bytes, what);
    assert_eq!(decoded.len(), 1, "{what}: {decoded:?}");
    decoded[0]
}

/// Operand `op` is the vector register `n` (YMM when `ymm`, else XMM).
fn intr22_vec(i: &Instruction, op: u32, ymm: bool, n: u8, what: &str) {
    assert_eq!(
        i.op_kind(op),
        OpKind::Register,
        "{what}: operand {op} is not a register"
    );
    let r = i.op_register(op);
    let class_ok = if ymm { r.is_ymm() } else { r.is_xmm() };
    assert!(
        class_ok && r.number() == usize::from(n),
        "{what}: operand {op} is {r:?}, wanted {}{n}",
        if ymm { "ymm" } else { "xmm" }
    );
}

/// Operand `op` is `[gpr64(base) + rdx * scale + disp]`.
fn intr22_mem(i: &Instruction, op: u32, base: u8, scale: u8, disp: i32, what: &str) {
    assert_eq!(
        i.op_kind(op),
        OpKind::Memory,
        "{what}: operand {op} is not memory"
    );
    let b = i.memory_base();
    assert!(
        b.is_gpr64() && b.number() == usize::from(base),
        "{what}: base is {b:?}, wanted gpr{base}"
    );
    assert_eq!(i.memory_index(), Register::RDX, "{what}: index register");
    assert_eq!(
        i.memory_index_scale(),
        u32::from(scale),
        "{what}: index scale"
    );
    let got = i64::from_ne_bytes(i.memory_displacement64().to_ne_bytes());
    assert_eq!(got, i64::from(disp), "{what}: displacement");
}

/// Operand `op` is the general register `want`.
fn intr22_gpr(i: &Instruction, op: u32, want: Register, what: &str) {
    assert_eq!(i.op_kind(op), OpKind::Register, "{what}: operand {op}");
    assert_eq!(i.op_register(op), want, "{what}: operand {op}");
}

#[test]
fn intr22_binop_encodings_decode_to_the_intended_instruction() {
    let table: &[(VecKind, VOp, Mn)] = &[
        (VecKind::Double, VOp::Add, Mn::Vaddpd),
        (VecKind::Double, VOp::Mul, Mn::Vmulpd),
        (VecKind::Double, VOp::Sub, Mn::Vsubpd),
        (VecKind::Double, VOp::Div, Mn::Vdivpd),
        (VecKind::Float, VOp::Add, Mn::Vaddps),
        (VecKind::Float, VOp::Mul, Mn::Vmulps),
        (VecKind::Float, VOp::Sub, Mn::Vsubps),
        (VecKind::Float, VOp::Div, Mn::Vdivps),
        (VecKind::Int, VOp::Add, Mn::Vpaddd),
        (VecKind::Int, VOp::Sub, Mn::Vpsubd),
        (VecKind::Int, VOp::Mul, Mn::Vpmulld),
        (VecKind::Long, VOp::Add, Mn::Vpaddq),
        (VecKind::Long, VOp::Sub, Mn::Vpsubq),
        (VecKind::Byte, VOp::Add, Mn::Vpaddb),
        (VecKind::Byte, VOp::Sub, Mn::Vpsubb),
        (VecKind::Short, VOp::Add, Mn::Vpaddw),
        (VecKind::Short, VOp::Sub, Mn::Vpsubw),
        (VecKind::Short, VOp::Mul, Mn::Vpmullw),
        // The bitwise ops are lane-width agnostic: VPAND / VPOR / VPXOR.
        (VecKind::Int, VOp::And, Mn::Vpand),
        (VecKind::Int, VOp::Or, Mn::Vpor),
        (VecKind::Int, VOp::Xor, Mn::Vpxor),
        (VecKind::Long, VOp::And, Mn::Vpand),
        (VecKind::Long, VOp::Or, Mn::Vpor),
        (VecKind::Long, VOp::Xor, Mn::Vpxor),
        (VecKind::Byte, VOp::And, Mn::Vpand),
        (VecKind::Short, VOp::Or, Mn::Vpor),
        (VecKind::Short, VOp::Xor, Mn::Vpxor),
    ];
    let regs = [(0u8, 0u8), (3, 5), (9, 12), (15, 1), (1, 15), (8, 8)];
    let srcs = [
        Src::Reg(2),
        Src::Reg(7),
        Src::Reg(8),
        Src::Reg(15),
        Src::Mem {
            base: 0,
            disp: 0x10,
        },
        Src::Mem {
            base: 1,
            disp: 0x30,
        },
        Src::Mem {
            base: 8,
            disp: 0x10,
        },
        Src::Mem {
            base: 9,
            disp: 0x70,
        },
    ];
    for &(kind, op, want) in table {
        for &(dst, src1) in &regs {
            for &src2 in &srcs {
                let mut o = Vec::new();
                emit_binop(&mut o, kind, op, dst, src1, src2);
                let what = format!("{kind:?} {op:?} dst={dst} src1={src1}");
                let i = intr22_one(&o, &what);
                assert_eq!(i.mnemonic(), want, "{what}");
                assert_eq!(i.op_count(), 3, "{what}");
                intr22_vec(&i, 0, true, dst, &what);
                intr22_vec(&i, 1, true, src1, &what);
                match src2 {
                    Src::Reg(r) => intr22_vec(&i, 2, true, r, &what),
                    Src::Mem { base, disp } => {
                        intr22_mem(&i, 2, base, kind.elem_bytes(), i32::from(disp), &what)
                    }
                }
            }
        }
    }
}

#[test]
fn intr22_load_store_move_zero_broadcast_decode_to_the_intended_instruction() {
    let all = [
        VecKind::Double,
        VecKind::Float,
        VecKind::Int,
        VecKind::Long,
        VecKind::Byte,
        VecKind::Short,
    ];
    for kind in all {
        let (load_mn, store_mn, bcast_mn) = match kind {
            VecKind::Double => (Mn::Vmovupd, Mn::Vmovupd, Mn::Vbroadcastsd),
            VecKind::Float => (Mn::Vmovups, Mn::Vmovups, Mn::Vbroadcastss),
            VecKind::Int => (Mn::Vmovdqu, Mn::Vmovdqu, Mn::Vpbroadcastd),
            VecKind::Long => (Mn::Vmovdqu, Mn::Vmovdqu, Mn::Vpbroadcastq),
            VecKind::Byte => (Mn::Vmovdqu, Mn::Vmovdqu, Mn::Vpbroadcastb),
            VecKind::Short => (Mn::Vmovdqu, Mn::Vmovdqu, Mn::Vpbroadcastw),
        };
        for (reg, base, disp) in [
            (0u8, 0u8, 0x10i8),
            (9, 1, 0x30),
            (15, 8, 0x10),
            (3, 9, 0x70),
        ] {
            let what = format!("{kind:?} reg={reg} base={base}");
            let mut o = Vec::new();
            emit_load(&mut o, kind, reg, base, disp);
            let i = intr22_one(&o, &what);
            assert_eq!(i.mnemonic(), load_mn, "load {what}");
            assert_eq!(i.op_count(), 2, "load {what}");
            intr22_vec(&i, 0, true, reg, &what);
            intr22_mem(&i, 1, base, kind.elem_bytes(), i32::from(disp), &what);

            let mut o = Vec::new();
            emit_store(&mut o, kind, reg, base, disp);
            let i = intr22_one(&o, &what);
            assert_eq!(i.mnemonic(), store_mn, "store {what}");
            assert_eq!(i.op_count(), 2, "store {what}");
            intr22_mem(&i, 0, base, kind.elem_bytes(), i32::from(disp), &what);
            intr22_vec(&i, 1, true, reg, &what);
        }
        for (dst, src) in [(0u8, 0u8), (15, 0), (3, 9), (9, 3), (8, 15)] {
            let what = format!("{kind:?} broadcast dst={dst} src={src}");
            let mut o = Vec::new();
            emit_broadcast(&mut o, kind, dst, src);
            let i = intr22_one(&o, &what);
            assert_eq!(i.mnemonic(), bcast_mn, "{what}");
            assert_eq!(i.op_count(), 2, "{what}");
            intr22_vec(&i, 0, true, dst, &what);
            intr22_vec(&i, 1, false, src, &what);
        }
    }
    // VMOVAPS ymm, ymm (nothing for a self-move); VPXOR r, r, r.
    for (dst, src) in [(0u8, 15u8), (15, 0), (3, 9), (9, 3), (8, 7)] {
        let what = format!("move dst={dst} src={src}");
        let mut o = Vec::new();
        emit_move(&mut o, dst, src);
        let i = intr22_one(&o, &what);
        assert_eq!(i.mnemonic(), Mn::Vmovaps, "{what}");
        intr22_vec(&i, 0, true, dst, &what);
        intr22_vec(&i, 1, true, src, &what);
    }
    let mut o = Vec::new();
    emit_move(&mut o, 4, 4);
    assert!(o.is_empty(), "a self-move emits nothing");
    for r in [0u8, 1, 7, 8, 15] {
        let what = format!("zero r={r}");
        let mut o = Vec::new();
        emit_zero(&mut o, r);
        let i = intr22_one(&o, &what);
        assert_eq!(i.mnemonic(), Mn::Vpxor, "{what}");
        for op in 0..3 {
            intr22_vec(&i, op, true, r, &what);
        }
    }
}

#[test]
fn intr22_shift_encodings_decode_to_the_intended_instruction() {
    let table: &[(VecKind, VShift, Mn)] = &[
        (VecKind::Int, VShift::Shl, Mn::Vpslld),
        (VecKind::Int, VShift::Sar, Mn::Vpsrad),
        (VecKind::Int, VShift::Shr, Mn::Vpsrld),
        (VecKind::Long, VShift::Shl, Mn::Vpsllq),
        (VecKind::Long, VShift::Shr, Mn::Vpsrlq),
    ];
    for &(kind, sh, want) in table {
        // By an immediate: `v<op> ymm_dst, ymm_src, imm8`.
        for (dst, src) in [(0u8, 0u8), (1, 1), (9, 9), (3, 12), (14, 2)] {
            for imm in [1u8, 7, 31] {
                let what = format!("{kind:?} {sh:?} imm dst={dst} src={src} imm={imm}");
                let mut o = Vec::new();
                assert!(emit_shift(&mut o, kind, sh, dst, src, imm).is_some());
                let i = intr22_one(&o, &what);
                assert_eq!(i.mnemonic(), want, "{what}");
                assert_eq!(i.op_count(), 3, "{what}");
                intr22_vec(&i, 0, true, dst, &what);
                intr22_vec(&i, 1, true, src, &what);
                assert_eq!(i.op_kind(2), OpKind::Immediate8, "{what}");
                assert_eq!(i.immediate8(), imm, "{what}");
            }
        }
        // By a count register: `v<op> ymm_dst, ymm_src, xmm_count`.
        for (dst, src, count) in [(0u8, 1u8, 2u8), (9, 10, 11), (3, 3, 15), (14, 0, 8)] {
            let what = format!("{kind:?} {sh:?} var dst={dst} src={src} count={count}");
            let mut o = Vec::new();
            assert!(emit_shift_var(&mut o, kind, sh, dst, src, count).is_some());
            let i = intr22_one(&o, &what);
            assert_eq!(i.mnemonic(), want, "{what}");
            assert_eq!(i.op_count(), 3, "{what}");
            intr22_vec(&i, 0, true, dst, &what);
            intr22_vec(&i, 1, true, src, &what);
            intr22_vec(&i, 2, false, count, &what);
        }
    }
    // AVX2 has no VPSRAQ and no sub-word shifts.
    let mut o = Vec::new();
    assert!(emit_shift(&mut o, VecKind::Long, VShift::Sar, 0, 0, 1).is_none());
    assert!(emit_shift_var(&mut o, VecKind::Long, VShift::Sar, 0, 0, 1).is_none());
    assert!(emit_shift(&mut o, VecKind::Byte, VShift::Shl, 0, 0, 1).is_none());
    assert!(o.is_empty());
}

#[test]
fn intr22_widening_loads_decode_to_the_intended_instruction() {
    let h = i8::try_from(cratonvm_types::HEADER_SIZE).unwrap();
    let table: &[(VecKind, MemKind, Mn)] = &[
        (VecKind::Int, MemKind::Byte, Mn::Vpmovsxbd),
        (VecKind::Int, MemKind::Short, Mn::Vpmovsxwd),
        (VecKind::Int, MemKind::Char, Mn::Vpmovzxwd),
        (VecKind::Short, MemKind::Byte, Mn::Vpmovsxbw),
        (VecKind::Long, MemKind::Int, Mn::Vpmovsxdq),
        (VecKind::Long, MemKind::Short, Mn::Vpmovsxwq),
        (VecKind::Long, MemKind::Char, Mn::Vpmovzxwq),
        (VecKind::Long, MemKind::Byte, Mn::Vpmovsxbq),
        (VecKind::Double, MemKind::Float, Mn::Vcvtps2pd),
    ];
    for &(kind, src, want) in table {
        for (dst, base) in [(0u8, 0u8), (9, 1), (15, 8), (3, 9)] {
            let what = format!("{kind:?} <- {src:?} dst={dst} base={base}");
            let mut o = Vec::new();
            assert!(emit_widen_load(&mut o, kind, src, dst, base, h).is_some());
            let i = intr22_one(&o, &what);
            assert_eq!(i.mnemonic(), want, "{what}");
            assert_eq!(i.op_count(), 2, "{what}");
            intr22_vec(&i, 0, true, dst, &what);
            // The element address scales `j` by the SOURCE width; the
            // first step (`disp == HEADER_SIZE`) sits at the payload start.
            intr22_mem(&i, 1, base, mem_bytes(src), i32::from(h), &what);
        }
    }
}

#[test]
fn intr22_reduction_fold_and_long_multiply_decode_to_the_documented_sequences() {
    // The horizontal fold: extract, add, shuffle, add, (shuffle, add), move out.
    for (acc, tmp) in [(1u8, 0u8), (0, 1), (2, 9), (9, 3), (14, 15), (8, 7)] {
        for kind in [VecKind::Int, VecKind::Long] {
            let what = format!("fold {kind:?} acc={acc} tmp={tmp}");
            let code = fold_code(kind, acc, tmp).unwrap();
            let d = intr22_decode(&code, &what);
            let (add, last, shuffles, out_reg) = if kind == VecKind::Int {
                (Mn::Vpaddd, Mn::Vmovd, vec![0x4Eu8, 0xB1], Register::EAX)
            } else {
                (Mn::Vpaddq, Mn::Vmovq, vec![0x4E], Register::RAX)
            };
            assert_eq!(d.len(), 2 + 2 * shuffles.len() + 1, "{what}: {d:?}");
            // VEXTRACTI128 xmm_tmp, ymm_acc, 1
            assert_eq!(d[0].mnemonic(), Mn::Vextracti128, "{what}");
            intr22_vec(&d[0], 0, false, tmp, &what);
            intr22_vec(&d[0], 1, true, acc, &what);
            assert_eq!(d[0].immediate8(), 1, "{what}");
            // VPADDx xmm_acc, xmm_acc, xmm_tmp
            let padd = |i: &Instruction| {
                assert_eq!(i.mnemonic(), add, "{what}");
                intr22_vec(i, 0, false, acc, &what);
                intr22_vec(i, 1, false, acc, &what);
                intr22_vec(i, 2, false, tmp, &what);
            };
            padd(&d[1]);
            for (k, imm) in shuffles.iter().enumerate() {
                // VPSHUFD xmm_tmp, xmm_acc, imm
                let s = &d[2 + 2 * k];
                assert_eq!(s.mnemonic(), Mn::Vpshufd, "{what}");
                intr22_vec(s, 0, false, tmp, &what);
                intr22_vec(s, 1, false, acc, &what);
                assert_eq!(s.immediate8(), *imm, "{what}");
                padd(&d[3 + 2 * k]);
            }
            // VMOVD eax, xmm_acc / VMOVQ rax, xmm_acc
            let m = &d[d.len() - 1];
            assert_eq!(m.mnemonic(), last, "{what}");
            intr22_gpr(m, 0, out_reg, &what);
            intr22_vec(m, 1, false, acc, &what);
        }
    }
    // `a * b` mod 2^64 from three VPMULUDQ (`emit_long_mul`'s listing).
    for (dst, b, t1, t2) in [
        (0u8, 1u8, 2u8, 3u8),
        (4, 5, 6, 7),
        (9, 10, 11, 12),
        (15, 8, 0, 1),
    ] {
        let what = format!("longmul dst={dst} b={b} t1={t1} t2={t2}");
        let mut o = Vec::new();
        assert!(emit_long_mul(&mut o, dst, b, t1, t2).is_some());
        let d = intr22_decode(&o, &what);
        assert_eq!(d.len(), 8, "{what}: {d:?}");
        let expect: [(Mn, u8, u8, Option<u8>); 8] = [
            (Mn::Vpsrlq, t1, dst, None), // t1 = dst >> 32 (imm)
            (Mn::Vpmuludq, t1, t1, Some(b)),
            (Mn::Vpsrlq, t2, b, None), // t2 = b >> 32 (imm)
            (Mn::Vpmuludq, t2, t2, Some(dst)),
            (Mn::Vpaddq, t1, t1, Some(t2)),
            (Mn::Vpsllq, t1, t1, None), // t1 <<= 32 (imm)
            (Mn::Vpmuludq, dst, dst, Some(b)),
            (Mn::Vpaddq, dst, dst, Some(t1)),
        ];
        for (n, (mn, r0, r1, r2)) in expect.iter().enumerate() {
            assert_eq!(d[n].mnemonic(), *mn, "{what} #{n}");
            assert_eq!(d[n].op_count(), 3, "{what} #{n}");
            intr22_vec(&d[n], 0, true, *r0, &what);
            intr22_vec(&d[n], 1, true, *r1, &what);
            match r2 {
                Some(r) => intr22_vec(&d[n], 2, true, *r, &what),
                None => {
                    assert_eq!(d[n].op_kind(2), OpKind::Immediate8, "{what} #{n}");
                    assert_eq!(d[n].immediate8(), 32, "{what} #{n}");
                }
            }
        }
    }
}

#[test]
fn intr22_narrowing_store_and_count_loads_decode_to_the_documented_sequences() {
    let h = i8::try_from(cratonvm_types::HEADER_SIZE).unwrap();
    for (dst_kind, w, unpack, store) in [
        (MemKind::Byte, 1u8, Mn::Vpunpckldq, Mn::Vmovq),
        (MemKind::Short, 2, Mn::Vpunpcklqdq, Mn::Vmovdqu),
        (MemKind::Char, 2, Mn::Vpunpcklqdq, Mn::Vmovdqu),
    ] {
        for (src, u, mask, base) in [
            (0u8, 1u8, 2u8, 0u8),
            (3, 4, 5, 1),
            (9, 10, 11, 8),
            (15, 0, 7, 9),
        ] {
            let what = format!("narrow {dst_kind:?} src={src} u={u} mask={mask} base={base}");
            let mut o = Vec::new();
            assert!(emit_narrow_store(&mut o, dst_kind, src, u, mask, base, h).is_some());
            let d = intr22_decode(&o, &what);
            assert_eq!(d.len(), 4, "{what}: {d:?}");
            // VPSHUFB ymm_src, ymm_src, ymm_mask
            assert_eq!(d[0].mnemonic(), Mn::Vpshufb, "{what}");
            intr22_vec(&d[0], 0, true, src, &what);
            intr22_vec(&d[0], 1, true, src, &what);
            intr22_vec(&d[0], 2, true, mask, &what);
            // VEXTRACTI128 xmm_u, ymm_src, 1
            assert_eq!(d[1].mnemonic(), Mn::Vextracti128, "{what}");
            intr22_vec(&d[1], 0, false, u, &what);
            intr22_vec(&d[1], 1, true, src, &what);
            assert_eq!(d[1].immediate8(), 1, "{what}");
            // VPUNPCKLDQ / VPUNPCKLQDQ xmm_src, xmm_src, xmm_u
            assert_eq!(d[2].mnemonic(), unpack, "{what}");
            intr22_vec(&d[2], 0, false, src, &what);
            intr22_vec(&d[2], 1, false, src, &what);
            intr22_vec(&d[2], 2, false, u, &what);
            // VMOVQ m64, xmm / VMOVDQU m128, xmm at the element scale of the
            // NARROW array; the first step sits at the payload start.
            assert_eq!(d[3].mnemonic(), store, "{what}");
            intr22_mem(&d[3], 0, base, w, i32::from(h), &what);
            intr22_vec(&d[3], 1, false, src, &what);
        }
    }
    // `AND R11D, 31|63` then `VMOVD xmm_count, R11D`.
    for (kind, mask) in [(VecKind::Int, 31u64), (VecKind::Long, 63)] {
        for count in [0u8, 3, 8, 15] {
            let what = format!("count load {kind:?} count={count}");
            let d = intr22_decode(&count_load_code(kind, count), &what);
            assert_eq!(d.len(), 2, "{what}: {d:?}");
            assert_eq!(d[0].mnemonic(), Mn::And, "{what}");
            intr22_gpr(&d[0], 0, Register::R11D, &what);
            assert_eq!(d[0].immediate(1), mask, "{what}");
            assert_eq!(d[1].mnemonic(), Mn::Vmovd, "{what}");
            intr22_vec(&d[1], 0, false, count, &what);
            intr22_gpr(&d[1], 1, Register::R11D, &what);
        }
    }
    // `VMOVQ xmm, r11` then `VPBROADCASTQ ymm, xmm`.
    for reg in [0u8, 5, 9, 15] {
        let what = format!("narrow mask load reg={reg}");
        let d = intr22_decode(&narrow_mask_load_code(reg), &what);
        assert_eq!(d.len(), 2, "{what}: {d:?}");
        assert_eq!(d[0].mnemonic(), Mn::Vmovq, "{what}");
        intr22_vec(&d[0], 0, false, reg, &what);
        intr22_gpr(&d[0], 1, Register::R11, &what);
        assert_eq!(d[1].mnemonic(), Mn::Vpbroadcastq, "{what}");
        intr22_vec(&d[1], 0, true, reg, &what);
        intr22_vec(&d[1], 1, false, reg, &what);
    }
}

#[test]
fn a_step_with_two_registers_computes_c_plus_x_times_b() {
    // c[j] = c[j] + x * b[j], bases [c, b], invariants [x], with the tree
    // on the left: the shape `build_expr` gives a double add under the
    // opt-in `CRATONVM_JIT_FP_NAN_OPERAND_ORDER=1` (r22 w6c); `gen` never
    // swaps a tree-left plan, so the bytes hold with the switch on or off.
    let plan = plan_of(
        VecKind::Double,
        VExpr::Bin(
            VOp::Add,
            Box::new(VExpr::Bin(
                VOp::Mul,
                Box::new(VExpr::Inv(0)),
                Box::new(VExpr::Load(1)),
            )),
            Box::new(VExpr::Load(0)),
        ),
        2,
        1,
    );
    let regs = StepRegs {
        temps: &[0],
        inv_regs: &[1],
        acc: None,
        nan: None,
    };
    let code = step_code(&plan, &regs, 0x10).expect("one temporary suffices");
    assert_eq!(
        code,
        [
            0xC4, 0xE1, 0x7C, 0x28, 0xC1, // vmovaps ymm0, ymm1
            0xC4, 0xE1, 0x7D, 0x59, 0x44, 0xD1, 0x10, // vmulpd ymm0, ymm0, [rcx+rdx*8+16]
            0xC4, 0xE1, 0x7D, 0x58, 0x44, 0xD0, 0x10, // vaddpd ymm0, ymm0, [rax+rdx*8+16]
            0xC4, 0xE1, 0x7D, 0x11, 0x44, 0xD0, 0x10, // vmovupd [rax+rdx*8+16], ymm0
        ]
    );
    // `c - x * b` does not commute: it needs a second temporary.
    let mut sub = plan.clone();
    sub.value = VExpr::Bin(
        VOp::Sub,
        Box::new(VExpr::Load(0)),
        Box::new(VExpr::Bin(
            VOp::Mul,
            Box::new(VExpr::Inv(0)),
            Box::new(VExpr::Load(1)),
        )),
    );
    assert!(step_code(&sub, &regs, 0x10).is_none());
    assert!(step_code(
        &sub,
        &StepRegs {
            temps: &[0, 8],
            inv_regs: &[1],
            acc: None,
            nan: None,
        },
        0x10
    )
    .is_some());
}

/// A reduction step adds into the accumulator and stores nothing; a leaf
/// addend needs no temporary at all, and a missing accumulator refuses.
#[test]
fn a_reduction_step_adds_into_the_accumulator() {
    let mut plan = plan_of(VecKind::Int, VExpr::Load(0), 1, 0);
    plan.reduce = Some(VecReduce {
        acc: 30,
        op: RedOp::Add,
    });
    let regs = StepRegs {
        temps: &[],
        inv_regs: &[],
        acc: Some(1),
        nan: None,
    };
    let code = step_code(&plan, &regs, 0x10).expect("a leaf needs no temporary");
    // vpaddd ymm1, ymm1, [rax + rdx*4 + 0x10]
    assert_eq!(code, [0xC4, 0xE1, 0x75, 0xFE, 0x4C, 0x90, 0x10]);
    let none = StepRegs {
        temps: &[0],
        inv_regs: &[],
        acc: None,
        nan: None,
    };
    assert!(step_code(&plan, &none, 0x10).is_none());
    // `s -= a[j] * b[j]`: one temporary, then VPSUBD into the accumulator.
    plan.value = VExpr::Bin(VOp::Mul, Box::new(VExpr::Load(0)), Box::new(VExpr::Load(1)));
    plan.bases.push(11);
    plan.reduce = Some(VecReduce {
        acc: 30,
        op: RedOp::Sub,
    });
    let regs = StepRegs {
        temps: &[0],
        inv_regs: &[],
        acc: Some(1),
        nan: None,
    };
    let code = step_code(&plan, &regs, 0x10).expect("one temporary");
    assert_eq!(
        code,
        [
            0xC4, 0xE1, 0x7E, 0x6F, 0x44, 0x90, 0x10, // vmovdqu ymm0, [rax+rdx*4+16]
            0xC4, 0xE2, 0x7D, 0x40, 0x44, 0x91, 0x10, // vpmulld ymm0, ymm0, [rcx+rdx*4+16]
            0xC4, 0xE1, 0x75, 0xFA, 0xC8, // vpsubd ymm1, ymm1, ymm0
        ]
    );
}

// ── Executed: the step bytes on the host CPU (AVX2 only) ─────────────

/// `CALL_ARG_REGS[0]`: where the harness's one pointer argument arrives.
#[cfg(windows)]
const ARG0: u8 = 1; // RCX
#[cfg(not(windows))]
const ARG0: u8 = 7; // RDI

/// A fake array: `len` in the VM's length word, `data` from
/// `HEADER_SIZE`, and 64 guard bytes past the end that no step may touch.
pub(super) fn fake_array(elem: usize, data: &[u8]) -> Vec<u8> {
    let h = cratonvm_types::HEADER_SIZE;
    let mut v = vec![0xA5u8; h + data.len() + 64];
    let len = (data.len() / elem) as u32;
    let lo = cratonvm_types::ARRAY_LENGTH_OFFSET;
    v[lo..lo + 4].copy_from_slice(&len.to_le_bytes());
    v[h..h + data.len()].copy_from_slice(data);
    v
}

/// Run `body` as `extern "C" fn(*mut [u64; 8])` with RAX/RCX/R8/R9 =
/// `args[0..4]`, RDX = `args[4]` (the element index `j`) and, on return,
/// `args[5]` = RAX. `body` may touch only those GPRs, R10/R11 and
/// XMM0-XMM3 (volatile on both ABIs). `None` without AVX2.
pub(super) fn run_on_host(body: &[u8], args: &mut [u64; 8]) -> Option<()> {
    if !crate::x64::has_avx2() {
        return None;
    }
    let mut buf = crate::ExecutableBuffer::new(4096).expect("executable buffer");
    // MOV R11, ARG0
    buf.emit(&[0x49, 0x89, 0xC3 | ((ARG0 & 7) << 3)]);
    // MOV RAX,[R11]; MOV RCX,[R11+8]; MOV R8,[R11+16]; MOV R9,[R11+24];
    // MOV RDX,[R11+32].
    buf.emit(&[0x49, 0x8B, 0x03]);
    buf.emit(&[0x49, 0x8B, 0x4B, 0x08]);
    buf.emit(&[0x4D, 0x8B, 0x43, 0x10]);
    buf.emit(&[0x4D, 0x8B, 0x4B, 0x18]);
    buf.emit(&[0x49, 0x8B, 0x53, 0x20]);
    buf.emit(body);
    buf.emit(&[0x49, 0x89, 0x43, 0x28]); // MOV [R11+40], RAX
    buf.emit(&[0xC5, 0xF8, 0x77]); // VZEROUPPER
    buf.emit(&[0xC3]); // RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: the buffer is a complete `extern "C" fn(*mut u64)` that
    // reads its argument block, touches only volatile registers and the
    // fake arrays the block points at, and returns.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(*mut u64)>(buf.as_ptr()) };
    f(args.as_mut_ptr());
    Some(())
}

pub(super) fn disp0() -> i8 {
    i8::try_from(cratonvm_types::HEADER_SIZE).expect("header fits a disp8")
}

/// `c[j] = ((a[j] << 3) ^ (a[j] >>> 29)) & k` over eight int lanes,
/// executed, against the scalar Java answer (the count masking included:
/// a planner-masked `<< 35` is `<< 3`).
#[test]
fn an_int_bitops_step_executes_like_java() {
    let a: Vec<i32> = (0..8i32)
        .map(|i| i.wrapping_mul(0x1357_9BDF).wrapping_sub(i.wrapping_shl(28)))
        .collect();
    let k = 0x7FF0_0FF7_i32;
    let mut c = fake_array(4, &[0u8; 32]);
    let a_arr = fake_array(4, &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
    let kbuf: Vec<u8> = (0..8).flat_map(|_| k.to_le_bytes()).collect();
    let k_arr = fake_array(4, &kbuf);
    // bases [c, a, kk]: k as a third "array" keeps the test register-free.
    let plan = plan_of(
        VecKind::Int,
        VExpr::Bin(
            VOp::And,
            Box::new(VExpr::Bin(
                VOp::Xor,
                Box::new(VExpr::Shift(VShift::Shl, 35 & 31, Box::new(VExpr::Load(1)))),
                Box::new(VExpr::Shift(VShift::Shr, 29, Box::new(VExpr::Load(1)))),
            )),
            Box::new(VExpr::Load(2)),
        ),
        3,
        0,
    );
    let regs = StepRegs {
        temps: &[0, 1, 2],
        inv_regs: &[],
        acc: None,
        nan: None,
    };
    let code = step_code(&plan, &regs, disp0()).expect("three temporaries");
    let mut args = [
        c.as_mut_ptr() as u64,
        a_arr.as_ptr() as u64,
        k_arr.as_ptr() as u64,
        0,
        0,
        0,
        0,
        0,
    ];
    if run_on_host(&code, &mut args).is_none() {
        return;
    }
    let h = cratonvm_types::HEADER_SIZE;
    for (i, &x) in a.iter().enumerate() {
        // Java's `x << 35` is `x << (35 & 31)`.
        let want = (x.wrapping_shl(35) ^ ((x as u32) >> 29) as i32) & k;
        let got = i32::from_le_bytes(c[h + 4 * i..h + 4 * i + 4].try_into().unwrap());
        assert_eq!(got, want, "lane {i}");
    }
    assert!(c[h + 32..].iter().all(|&b| b == 0xA5), "a store past the step");
}

/// `c[j] = (byte)((a[j] ^ b[j]) + a[j])` over 32 byte lanes, executed,
/// against Java's int arithmetic truncated by `bastore`.
#[test]
fn a_byte_lane_step_executes_like_java() {
    let a: Vec<u8> = (0..32u32).map(|i| (i * 37 + 200) as u8).collect();
    let b: Vec<u8> = (0..32u32).map(|i| (i * 91 + 7) as u8).collect();
    let mut c = fake_array(1, &[0u8; 32]);
    let a_arr = fake_array(1, &a);
    let b_arr = fake_array(1, &b);
    let plan = plan_of(
        VecKind::Byte,
        VExpr::Bin(
            VOp::Add,
            Box::new(VExpr::Bin(
                VOp::Xor,
                Box::new(VExpr::Load(1)),
                Box::new(VExpr::Load(2)),
            )),
            Box::new(VExpr::Load(1)),
        ),
        3,
        0,
    );
    let regs = StepRegs {
        temps: &[0, 1],
        inv_regs: &[],
        acc: None,
        nan: None,
    };
    let code = step_code(&plan, &regs, disp0()).expect("two temporaries");
    let mut args = [
        c.as_mut_ptr() as u64,
        a_arr.as_ptr() as u64,
        b_arr.as_ptr() as u64,
        0,
        0,
        0,
        0,
        0,
    ];
    if run_on_host(&code, &mut args).is_none() {
        return;
    }
    let h = cratonvm_types::HEADER_SIZE;
    for i in 0..32 {
        // Java: baload sign-extends, the int ops, bastore keeps the low byte.
        let (x, y) = (i32::from(a[i] as i8), i32::from(b[i] as i8));
        let want = ((x ^ y) + x) as u8;
        assert_eq!(c[h + i], want, "lane {i}");
    }
    assert!(c[h + 32..].iter().all(|&b| b == 0xA5), "a store past the step");
}

/// `s += a[j] * b[j]` (int, wrapping), two steps at `j = 0` and `j = 8`
/// into one accumulator, folded: the scalar wrapped dot product exactly.
#[test]
fn an_int_reduction_folds_to_the_wrapped_scalar_sum() {
    let a: Vec<i32> = (0..16).map(|i| i32::MAX - i * 977).collect();
    let b: Vec<i32> = (0..16).map(|i| 3 + i * 65_537).collect();
    let a_arr = fake_array(4, &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
    let b_arr = fake_array(4, &b.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
    let mut plan = plan_of(
        VecKind::Int,
        VExpr::Bin(VOp::Mul, Box::new(VExpr::Load(0)), Box::new(VExpr::Load(1))),
        2,
        0,
    );
    plan.reduce = Some(VecReduce {
        acc: 30,
        op: RedOp::Add,
    });
    let regs = StepRegs {
        temps: &[0],
        inv_regs: &[],
        acc: Some(1),
        nan: None,
    };
    let d0 = disp0();
    let mut body = Vec::new();
    emit_zero(&mut body, 1);
    body.extend(step_code(&plan, &regs, d0).unwrap());
    // The second step at the next 32 bytes (the main loop's unrolled form).
    body.extend(step_code(&plan, &regs, d0 + 32).unwrap());
    body.extend(fold_code(VecKind::Int, 1, 0).unwrap());
    let mut args = [a_arr.as_ptr() as u64, b_arr.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
    if run_on_host(&body, &mut args).is_none() {
        return;
    }
    let want = a
        .iter()
        .zip(&b)
        .fold(0i32, |s, (x, y)| s.wrapping_add(x.wrapping_mul(*y)));
    assert_eq!(args[5] as u32 as i32, want);
}

/// The long fold: four qword lanes of `a[j]` near `Long.MAX_VALUE` wrap.
#[test]
fn a_long_reduction_folds_to_the_wrapped_scalar_sum() {
    let a: Vec<i64> = (0..4).map(|i| i64::MAX - i * 3).collect();
    let a_arr = fake_array(8, &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
    let mut plan = plan_of(VecKind::Long, VExpr::Load(0), 1, 0);
    plan.reduce = Some(VecReduce {
        acc: 30,
        op: RedOp::Sub,
    });
    let regs = StepRegs {
        temps: &[0],
        inv_regs: &[],
        acc: Some(1),
        nan: None,
    };
    let mut body = Vec::new();
    emit_zero(&mut body, 1);
    body.extend(step_code(&plan, &regs, disp0()).unwrap());
    body.extend(fold_code(VecKind::Long, 1, 0).unwrap());
    let mut args = [a_arr.as_ptr() as u64, 0, 0, 0, 0, 0, 0, 0];
    if run_on_host(&body, &mut args).is_none() {
        return;
    }
    let want = a.iter().fold(0i64, |s, x| s.wrapping_sub(*x));
    assert_eq!(args[5] as i64, want);
}

// ── VEC21-5: min/max reductions ──────────────────────────────────────

/// The new encodings, each against the SDM form.
#[test]
fn the_r23_minmax_and_scan_encodings_match_the_sdm() {
    let mut o = Vec::new();
    // vpmaxsd ymm1, ymm1, [rax + rdx*4 + 0x10]   VEX.256.66.0F38 3D /r
    vex_src(
        &mut o,
        2,
        1,
        0x3D,
        1,
        1,
        Src::Mem {
            base: 0,
            disp: 0x10,
        },
        4,
    );
    assert_eq!(o, [0xC4, 0xE2, 0x75, 0x3D, 0x4C, 0x90, 0x10]);
    o.clear();
    // vpcmpgtq ymm3, ymm0, ymm1 ; vblendvpd ymm1, ymm1, ymm0, ymm3
    minmax_long(&mut o, true, true, 1, 0, 3);
    assert_eq!(
        o,
        [
            0xC4, 0xE2, 0x7D, 0x37, 0xD9, // vpcmpgtq ymm3, ymm0, ymm1
            0xC4, 0xE3, 0x75, 0x4B, 0xC8, 0x30, // vblendvpd ymm1, ymm1, ymm0, ymm3
        ]
    );
    o.clear();
    // The double max step body: cmpunordpd / orpd / maxpd / maxpd / andpd.
    minmax_fp(&mut o, true, true, true, 1, 0, 3, Some(2));
    assert_eq!(
        o,
        [
            0xC4, 0xE1, 0x7D, 0xC2, 0xD8, 0x03, // vcmpunordpd ymm3, ymm0, ymm0
            0xC4, 0xE1, 0x6D, 0x56, 0xD3, // vorpd ymm2, ymm2, ymm3
            0xC4, 0xE1, 0x75, 0x5F, 0xD8, // vmaxpd ymm3, ymm1, ymm0
            0xC4, 0xE1, 0x7D, 0x5F, 0xC9, // vmaxpd ymm1, ymm0, ymm1
            0xC4, 0xE1, 0x75, 0x54, 0xCB, // vandpd ymm1, ymm1, ymm3
        ]
    );
    // vptest ymm2, ymm2   VEX.256.66.0F38 17 /r
    assert_eq!(minmax_nan_test_code(2), [0xC4, 0xE2, 0x7D, 0x17, 0xD2]);
    // vpcmpeqb ymm0, ymm1, [rax + rdx*1 + 0x10] ; vpmovmskb r9d, ymm0 ;
    // test r9d, r9d ; jnz rel32
    let mut plan = plan_of(VecKind::Byte, VExpr::Load(0), 1, 1);
    plan.scan = Some(VecScan::single(
        ScanCmp::Eq,
        VExpr::Load(0),
        VExpr::Inv(0),
        false,
    ));
    let regs = StepRegs {
        temps: &[0],
        inv_regs: &[1],
        acc: None,
        nan: None,
    };
    let code = step_code(&plan, &regs, 0x10).expect("one temporary");
    assert_eq!(
        code,
        [
            0xC4, 0xE1, 0x75, 0x74, 0x44, 0x10, 0x10, // vpcmpeqb ymm0, ymm1, [rax+rdx+16]
            0xC4, 0x61, 0x7D, 0xD7, 0xC8, // vpmovmskb r9d, ymm0
            0x45, 0x85, 0xC9, // test r9d, r9d
            0x0F, 0x85, 0, 0, 0, 0, // jnz rel32
        ]
    );
    // The miss-mask exit of step 2 over int lanes.
    assert_eq!(
        scan_exit_code(VecKind::Int, true, 16),
        [
            0x41, 0xF7, 0xD1, // not r9d
            0x45, 0x0F, 0xBC, 0xC9, // bsf r9d, r9d
            0x41, 0xC1, 0xE9, 0x02, // shr r9d, 2
            0x4C, 0x01, 0xCA, // add rdx, r9
            0x48, 0x81, 0xC2, 16, 0, 0, 0, // add rdx, 16
        ]
    );
}

/// JDK 25 `Math.max(double, double)`, bit for bit.
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

/// JDK 25 `Math.min(double, double)`, bit for bit.
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

/// Two steps (`j = 0`, `j = 4`) of `m = Math.max/min(m, a[j])` over
/// double lanes, seeded with `seed` in every lane, on the host. Returns
/// (NaN seen, folded bits).
fn run_double_minmax(is_max: bool, seed: f64, a: &[f64; 8]) -> Option<(bool, u64)> {
    let a_arr = fake_array(
        8,
        &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
    );
    let s_arr = fake_array(
        8,
        &[seed; 4]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    );
    let mut plan = plan_of(VecKind::Double, VExpr::Load(0), 2, 0);
    plan.reduce = Some(VecReduce {
        acc: 30,
        op: if is_max { RedOp::Max } else { RedOp::Min },
    });
    let regs = StepRegs {
        temps: &[0, 3],
        inv_regs: &[],
        acc: Some(1),
        nan: Some(2),
    };
    let d0 = disp0();
    let mut pre = Vec::new();
    emit_load(&mut pre, VecKind::Double, 1, BASE_REGS[1], d0);
    pre.extend(minmax_nan_init_code(VecKind::Double, 1, 2));
    pre.extend(step_code(&plan, &regs, d0)?);
    pre.extend(step_code(&plan, &regs, d0 + 32)?);
    let mut flag = pre.clone();
    flag.extend(minmax_nan_test_code(2));
    flag.extend_from_slice(&[0x0F, 0x95, 0xC0]); // SETNZ AL
    flag.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // MOVZX RAX, AL
    let mut fold = pre;
    fold.extend(minmax_fold_code(VecKind::Double, is_max, 1, 0, 3)?);
    fold.extend_from_slice(&[0xC4, 0xE1, 0xF9, 0x7E, 0xC0]); // VMOVQ RAX, XMM0
    let mut args = [
        a_arr.as_ptr() as u64,
        s_arr.as_ptr() as u64,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    run_on_host(&flag, &mut args)?;
    let nan = args[5] != 0;
    run_on_host(&fold, &mut args)?;
    Some((nan, args[5]))
}

/// VEC21-5, executed: every NaN-free case is Java's sequential fold bit
/// for bit (`-0.0` vs `+0.0` ties in both orders and lane positions), and
/// every case with a NaN (element or seed, any payload) raises the
/// NaN-seen flag that discards the pre-loop.
#[test]
fn a_double_minmax_reduction_is_java_exact_or_flags_a_nan() {
    let pz = 0.0f64;
    let nz = -0.0f64;
    let odd_nan = f64::from_bits(0x7FF0_0000_0000_0123);
    let neg_nan = f64::from_bits(0xFFF8_0000_0000_0001);
    let cases: Vec<(f64, [f64; 8])> = vec![
        (
            f64::NEG_INFINITY,
            [1.0, -2.0, 3.5, 7.25, -0.5, 7.25, 0.0, 6.0],
        ),
        (nz, [nz, nz, nz, nz, nz, nz, nz, nz]),
        (nz, [nz, pz, nz, nz, nz, nz, nz, nz]),
        (pz, [nz, nz, nz, nz, nz, nz, nz, nz]),
        (nz, [nz, nz, nz, nz, nz, nz, nz, pz]),
        (pz, [pz, nz, pz, nz, pz, nz, pz, nz]),
        (
            f64::INFINITY,
            [
                f64::MAX,
                f64::MIN,
                f64::MIN_POSITIVE,
                -f64::MIN_POSITIVE,
                1e-310,
                -1e-310,
                3.0,
                -3.0,
            ],
        ),
        (1.0, [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
        (0.0, [1.0, f64::NAN, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]),
        (0.0, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, odd_nan]),
        (0.0, [neg_nan, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
        (f64::NAN, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
    ];
    for is_max in [true, false] {
        for (seed, a) in &cases {
            let Some((nan, bits)) = run_double_minmax(is_max, *seed, a) else {
                assert!(!crate::x64::has_avx2(), "the step or fold declined");
                return;
            };
            let any_nan = seed.is_nan() || a.iter().any(|x| x.is_nan());
            assert_eq!(nan, any_nan, "max={is_max} seed={seed} a={a:?}: NaN flag");
            if !any_nan {
                let want = a
                    .iter()
                    .fold(*seed, |m, &x| if is_max { jmax(m, x) } else { jmin(m, x) });
                assert_eq!(
                    bits,
                    want.to_bits(),
                    "max={is_max} seed={seed} a={a:?}: got {}",
                    f64::from_bits(bits)
                );
            }
        }
    }
}

/// `m = Math.min(m, a[j])` over float lanes (one step, eight lanes), and
/// the float fold's extra shuffle.
#[test]
fn a_float_min_reduction_orders_negative_zero_below_positive_zero() {
    let cases: [[f32; 8]; 3] = [
        [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -0.0],
        [5.0, -1.5, 3.0, -1.5, 2.0, 9.0, -1.25, 4.0],
        [-0.0, 0.0, -0.0, 0.0, 1.0, 2.0, 3.0, 0.0],
    ];
    for a in cases {
        let a_arr = fake_array(
            4,
            &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
        );
        let s_arr = fake_array(
            4,
            &[0.0f32; 8]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let mut plan = plan_of(VecKind::Float, VExpr::Load(0), 2, 0);
        plan.reduce = Some(VecReduce {
            acc: 30,
            op: RedOp::Min,
        });
        let regs = StepRegs {
            temps: &[0, 3],
            inv_regs: &[],
            acc: Some(1),
            nan: Some(2),
        };
        let d0 = disp0();
        let mut body = Vec::new();
        emit_load(&mut body, VecKind::Float, 1, BASE_REGS[1], d0);
        body.extend(minmax_nan_init_code(VecKind::Float, 1, 2));
        body.extend(step_code(&plan, &regs, d0).unwrap());
        body.extend(minmax_fold_code(VecKind::Float, false, 1, 0, 3).unwrap());
        body.extend_from_slice(&[0xC4, 0xE1, 0x79, 0x7E, 0xC0]); // VMOVD EAX, XMM0
        let mut args = [
            a_arr.as_ptr() as u64,
            s_arr.as_ptr() as u64,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        if run_on_host(&body, &mut args).is_none() {
            return;
        }
        let want = a
            .iter()
            .fold(0.0f32, |m, &x| jmin(f64::from(m), f64::from(x)) as f32);
        assert_eq!(args[5] as u32, want.to_bits(), "a={a:?}");
    }
}

/// `int` max and `long` min reductions, executed over two steps and folded,
/// against the scalar answer (MIN/MAX values in odd lanes).
#[test]
fn integer_minmax_reductions_fold_to_the_scalar_answer() {
    let a: Vec<i32> = vec![
        3,
        i32::MIN,
        -7,
        12,
        i32::MAX - 1,
        0,
        -1,
        5,
        8,
        9,
        -100,
        i32::MAX,
        4,
        4,
        2,
        i32::MIN,
    ];
    let a_arr = fake_array(
        4,
        &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
    );
    let seed = i32::MIN + 1;
    let s_arr = fake_array(
        4,
        &[seed; 8]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    );
    let mut plan = plan_of(VecKind::Int, VExpr::Load(0), 2, 0);
    plan.reduce = Some(VecReduce {
        acc: 30,
        op: RedOp::Max,
    });
    let regs = StepRegs {
        temps: &[0],
        inv_regs: &[],
        acc: Some(1),
        nan: None,
    };
    let d0 = disp0();
    let mut body = Vec::new();
    emit_load(&mut body, VecKind::Int, 1, BASE_REGS[1], d0);
    body.extend(step_code(&plan, &regs, d0).unwrap());
    body.extend(step_code(&plan, &regs, d0 + 32).unwrap());
    body.extend(minmax_fold_code(VecKind::Int, true, 1, 0, 0).unwrap());
    let mut args = [
        a_arr.as_ptr() as u64,
        s_arr.as_ptr() as u64,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    if run_on_host(&body, &mut args).is_none() {
        return;
    }
    assert_eq!(args[5] as u32 as i32, a.iter().fold(seed, |m, &x| m.max(x)));

    let l: Vec<i64> = vec![5, i64::MAX, -3, i64::MIN + 1, 7, 0, i64::MIN, 2];
    let l_arr = fake_array(
        8,
        &l.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
    );
    let lseed = 1i64;
    let ls_arr = fake_array(
        8,
        &[lseed; 4]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    );
    let mut plan = plan_of(VecKind::Long, VExpr::Load(0), 2, 0);
    plan.reduce = Some(VecReduce {
        acc: 30,
        op: RedOp::Min,
    });
    let regs = StepRegs {
        temps: &[0, 3],
        inv_regs: &[],
        acc: Some(1),
        nan: None,
    };
    let mut body = Vec::new();
    emit_load(&mut body, VecKind::Long, 1, BASE_REGS[1], d0);
    body.extend(step_code(&plan, &regs, d0).unwrap());
    body.extend(step_code(&plan, &regs, d0 + 32).unwrap());
    body.extend(minmax_fold_code(VecKind::Long, false, 1, 0, 3).unwrap());
    let mut args = [
        l_arr.as_ptr() as u64,
        ls_arr.as_ptr() as u64,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    run_on_host(&body, &mut args).unwrap();
    assert_eq!(args[5] as i64, l.iter().fold(lseed, |m, &x| m.min(x)));
}

// ── VEC21-8: early-exit scans ────────────────────────────────────────

/// One scan step at `j = 0` over 32 bytes of `a`, with the invariant
/// loaded from `inv` (an array of its broadcast lanes); the exit stub
/// moves RDX onto the hit. Returns the hit index, or -1.
fn run_scan(kind: VecKind, scan: VecScan, a: &[u8], inv: &[u8]) -> Option<i64> {
    let a_arr = fake_array(usize::from(kind.elem_bytes()), a);
    let i_arr = fake_array(usize::from(kind.elem_bytes()), inv);
    let miss = scan.mask_complemented();
    let mut plan = plan_of(kind, VExpr::Load(0), 1, 1);
    plan.scan = Some(scan);
    let regs = StepRegs {
        temps: &[0],
        inv_regs: &[1],
        acc: None,
        nan: None,
    };
    let d0 = disp0();
    let mut body = Vec::new();
    // The invariant's lanes (ymm1) from the second "array" (RCX).
    emit_load(&mut body, kind, 1, BASE_REGS[1], d0);
    let step = step_code(&plan, &regs, d0)?;
    body.extend(&step);
    let exit = scan_exit_code(kind, miss, 0);
    // not taken: MOV RAX, -1 ; JMP end.   taken: exit ; MOV RAX, RDX.
    let jnz_end = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF]);
    body.extend_from_slice(&[0xEB, u8::try_from(exit.len() + 3).unwrap()]);
    let rel = i32::try_from(body.len() - jnz_end).unwrap();
    body[jnz_end - 4..jnz_end].copy_from_slice(&rel.to_le_bytes());
    body.extend(&exit);
    body.extend_from_slice(&[0x48, 0x89, 0xD0]); // MOV RAX, RDX
    let mut args = [
        a_arr.as_ptr() as u64,
        i_arr.as_ptr() as u64,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    run_on_host(&body, &mut args)?;
    Some(args[5] as i64)
}

/// `a[j] == c` over byte lanes and `a[j] != c` over 16-bit lanes, every
/// hit position plus none; and `a[j] < c` (as `c > a[j]`) over int lanes
/// with MIN/MAX values.
#[test]
fn scan_steps_stop_exactly_at_the_first_hit() {
    let c = -56i8;
    for pos in 0..=32usize {
        let mut a: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(7)).collect();
        if pos < 32 {
            a[pos] = c as u8;
            if pos + 3 < 32 {
                a[pos + 3] = c as u8; // a later hit must not win
            }
        }
        let got = run_scan(
            VecKind::Byte,
            VecScan::single(ScanCmp::Eq, VExpr::Load(0), VExpr::Inv(0), false),
            &a,
            &[c as u8; 32],
        );
        let Some(got) = got else {
            assert!(!crate::x64::has_avx2(), "the scan step declined");
            return;
        };
        let want = a
            .iter()
            .position(|&x| x == c as u8)
            .map_or(-1, |p| p as i64);
        assert_eq!(got, want, "byte == at {pos}");
    }
    for pos in 0..=16usize {
        let mut a = vec![0xFFFFu16; 16];
        if pos < 16 {
            a[pos] = 0x7FFF;
        }
        let bytes: Vec<u8> = a.iter().flat_map(|v| v.to_le_bytes()).collect();
        let inv: Vec<u8> = [0xFFFFu16; 16]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let got = run_scan(
            VecKind::Short,
            VecScan::single(ScanCmp::Eq, VExpr::Load(0), VExpr::Inv(0), true),
            &bytes,
            &inv,
        )
        .unwrap();
        assert_eq!(
            got,
            if pos < 16 { pos as i64 } else { -1 },
            "short != at {pos}"
        );
    }
    let c = -5i32;
    for pos in 0..=8usize {
        let mut a = vec![i32::MAX, -5, 0, i32::MAX, -4, 7, -5, 100];
        if pos < 8 {
            a[pos] = i32::MIN;
        }
        let bytes: Vec<u8> = a.iter().flat_map(|v| v.to_le_bytes()).collect();
        let inv: Vec<u8> = [c; 8].iter().flat_map(|v| v.to_le_bytes()).collect();
        let got = run_scan(
            VecKind::Int,
            VecScan::single(ScanCmp::Gt, VExpr::Inv(0), VExpr::Load(0), false),
            &bytes,
            &inv,
        )
        .unwrap();
        let want = a.iter().position(|&x| x < c).map_or(-1, |p| p as i64);
        assert_eq!(got, want, "int < at {pos}");
    }
}
// ── VEC21-2 / -6: widening and narrowing lanes, variable counts ──

/// The residual items' encodings, each against the SDM form.
#[test]
fn the_vec21_residual_encodings_match_the_sdm() {
    let h = disp0() as u8;
    let mut o = Vec::new();
    // vpmovzxwd ymm0, [rcx + rdx*2 + H + 16]   VEX.256.66.0F38 33 /r (step 1)
    emit_widen_load(&mut o, VecKind::Int, MemKind::Char, 0, 1, disp0() + 32).unwrap();
    assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x33, 0x44, 0x51, h + 16]);
    o.clear();
    // vpmovsxbd ymm1, [rax + rdx*1 + H + 24]   VEX.256.66.0F38 21 /r (step 3)
    emit_widen_load(&mut o, VecKind::Int, MemKind::Byte, 1, 0, disp0() + 96).unwrap();
    assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x21, 0x4C, 0x10, h + 24]);
    o.clear();
    // vpmovsxdq ymm2, [r9 + rdx*4 + H]          VEX.256.66.0F38 25 /r
    emit_widen_load(&mut o, VecKind::Long, MemKind::Int, 2, 9, disp0()).unwrap();
    assert_eq!(o, [0xC4, 0xC2, 0x7D, 0x25, 0x54, 0x91, h]);
    o.clear();
    // vcvtps2pd ymm9, [r8 + rdx*4 + H]          VEX.256.0F 5A /r
    emit_widen_load(&mut o, VecKind::Double, MemKind::Float, 9, 8, disp0()).unwrap();
    assert_eq!(o, [0xC4, 0x41, 0x7C, 0x5A, 0x4C, 0x90, h]);
    o.clear();
    // No narrowing form, and no float -> int.
    assert!(emit_widen_load(&mut o, VecKind::Byte, MemKind::Short, 0, 0, disp0()).is_none());
    assert!(emit_widen_load(&mut o, VecKind::Int, MemKind::Float, 0, 0, disp0()).is_none());
    // vpslld ymm1, ymm2, xmm3                   VEX.256.66.0F F2 /r
    emit_shift_var(&mut o, VecKind::Int, VShift::Shl, 1, 2, 3).unwrap();
    assert_eq!(o, [0xC4, 0xE1, 0x6D, 0xF2, 0xCB]);
    o.clear();
    // vpsrlq ymm8, ymm8, xmm15                  VEX.256.66.0F D3 /r
    emit_shift_var(&mut o, VecKind::Long, VShift::Shr, 8, 8, 15).unwrap();
    assert_eq!(o, [0xC4, 0x41, 0x3D, 0xD3, 0xC7]);
    o.clear();
    // vpsrad ymm0, ymm0, xmm1                   VEX.256.66.0F E2 /r
    emit_shift_var(&mut o, VecKind::Int, VShift::Sar, 0, 0, 1).unwrap();
    assert_eq!(o, [0xC4, 0xE1, 0x7D, 0xE2, 0xC1]);
    o.clear();
    assert!(emit_shift_var(&mut o, VecKind::Long, VShift::Sar, 0, 0, 1).is_none());
    // and r11d, 31 ; vmovd xmm3, r11d           VEX.128.66.0F.W0 6E /r
    assert_eq!(
        count_load_code(VecKind::Int, 3),
        [0x41, 0x83, 0xE3, 0x1F, 0xC4, 0xC1, 0x79, 0x6E, 0xDB]
    );
    // and r11d, 63 ; vmovd xmm9, r11d
    assert_eq!(
        count_load_code(VecKind::Long, 9),
        [0x41, 0x83, 0xE3, 0x3F, 0xC4, 0x41, 0x79, 0x6E, 0xCB]
    );
}

/// One element of `src` as Java's `xaload` (and `i2l` / `f2d`) widens it,
/// then truncated to a `kind` lane: the bits the step must store.
fn widened(kind: VecKind, src: MemKind, raw: &[u8]) -> Vec<u8> {
    let v: i64 = match src {
        MemKind::Byte => i64::from(raw[0] as i8),
        MemKind::Short => i64::from(i16::from_le_bytes([raw[0], raw[1]])),
        MemKind::Char => i64::from(u16::from_le_bytes([raw[0], raw[1]])),
        MemKind::Int => i64::from(i32::from_le_bytes(raw[..4].try_into().unwrap())),
        MemKind::Float => {
            let f = f32::from_bits(u32::from_le_bytes(raw[..4].try_into().unwrap()));
            return f64::from(f).to_bits().to_le_bytes().to_vec();
        }
        _ => unreachable!(),
    };
    v.to_le_bytes()[..usize::from(kind.elem_bytes())].to_vec()
}

/// Every widening pair, executed: two steps (`j = 0` and the next 32 bytes
/// of lanes, so the source displacement's scaling is checked) of
/// `c[j] = a[j]` with `a` narrower, against the scalar extension.
#[test]
fn widening_loads_execute_like_the_scalar_extension() {
    let pairs = [
        (VecKind::Int, MemKind::Byte),
        (VecKind::Int, MemKind::Short),
        (VecKind::Int, MemKind::Char),
        (VecKind::Short, MemKind::Byte),
        (VecKind::Long, MemKind::Int),
        (VecKind::Long, MemKind::Short),
        (VecKind::Long, MemKind::Char),
        (VecKind::Long, MemKind::Byte),
        (VecKind::Double, MemKind::Float),
    ];
    let h = cratonvm_types::HEADER_SIZE;
    for (kind, src) in pairs {
        let w = kind.lanes() as usize * 2;
        let sb = usize::from(mem_bytes(src));
        let eb = usize::from(kind.elem_bytes());
        let a: Vec<u8> = if src == MemKind::Float {
            (0..w)
                .flat_map(|i| {
                    let f = match i % 6 {
                        0 => f32::from_bits(0x7FC0_0042 + i as u32),
                        1 => -0.0,
                        2 => f32::from_bits(3),
                        3 => f32::NEG_INFINITY,
                        _ => i as f32 * -1.5 + 0.25,
                    };
                    f.to_bits().to_le_bytes()
                })
                .collect()
        } else {
            (0..w * sb)
                .map(|i| {
                    (i as u32).wrapping_mul(0x9E37_79B9).rotate_left(i as u32) as u8
                        ^ if i % 3 == 0 { 0x80 } else { 0 }
                })
                .collect()
        };
        let mut c = fake_array(eb, &vec![0u8; w * eb]);
        let a_arr = fake_array(sb, &a);
        let mut plan = plan_of(kind, VExpr::Load(1), 2, 0);
        plan.base_kinds[1] = src;
        let regs = StepRegs {
            temps: &[0, 1],
            inv_regs: &[],
            acc: None,
            nan: None,
        };
        let mut body = step_code(&plan, &regs, disp0()).expect("one temporary");
        body.extend(step_code(&plan, &regs, disp0() + 32).expect("one temporary"));
        let mut args = [
            c.as_mut_ptr() as u64,
            a_arr.as_ptr() as u64,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        if run_on_host(&body, &mut args).is_none() {
            return;
        }
        for i in 0..w {
            let want = widened(kind, src, &a[i * sb..(i + 1) * sb]);
            assert_eq!(
                &c[h + i * eb..h + (i + 1) * eb],
                &want[..],
                "{kind:?} <- {src:?} lane {i}"
            );
        }
        assert!(
            c[h + w * eb..].iter().all(|&b| b == 0xA5),
            "{kind:?} <- {src:?}: a store past the steps"
        );
    }
}

/// `c[j] = a[j] * b[j]` over `long` lanes: three `VPMULUDQ`, the wrapped
/// 64-bit product exactly, every half of each operand exercised.
#[test]
fn a_long_lane_multiply_is_the_wrapped_product() {
    let a: Vec<i64> = vec![
        -1,
        i64::MIN,
        0x1_0000_0001,
        0x7FFF_FFFF_FFFF_FFFF,
        3,
        -3,
        0xFFFF_FFFF,
        1 << 32,
    ];
    let b: Vec<i64> = vec![
        -1,
        3,
        0x1_0000_0001,
        0x7FFF_FFFF_FFFF_FFFF,
        i64::MIN + 7,
        0xFFFF_FFFF,
        0xFFFF_FFFF,
        -(1 << 32),
    ];
    let mut c = fake_array(8, &[0u8; 64]);
    let a_arr = fake_array(
        8,
        &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
    );
    let b_arr = fake_array(
        8,
        &b.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
    );
    let plan = plan_of(
        VecKind::Long,
        VExpr::Bin(VOp::Mul, Box::new(VExpr::Load(1)), Box::new(VExpr::Load(2))),
        3,
        0,
    );
    let regs = StepRegs {
        temps: &[0, 1, 2, 3],
        inv_regs: &[],
        acc: None,
        nan: None,
    };
    // The register shortage is a refusal, not wrong code.
    let short = StepRegs {
        temps: &[0, 1, 2],
        inv_regs: &[],
        acc: None,
        nan: None,
    };
    assert!(step_code(&plan, &short, disp0()).is_none());
    let mut body = step_code(&plan, &regs, disp0()).expect("four temporaries");
    body.extend(step_code(&plan, &regs, disp0() + 32).expect("four temporaries"));
    let mut args = [
        c.as_mut_ptr() as u64,
        a_arr.as_ptr() as u64,
        b_arr.as_ptr() as u64,
        0,
        0,
        0,
        0,
        0,
    ];
    if run_on_host(&body, &mut args).is_none() {
        return;
    }
    let h = cratonvm_types::HEADER_SIZE;
    for i in 0..8 {
        let got = i64::from_le_bytes(c[h + 8 * i..h + 8 * i + 8].try_into().unwrap());
        assert_eq!(got, a[i].wrapping_mul(b[i]), "lane {i}");
    }
}

/// VEC21-6, executed: `c[j] = (a[j] << k) ^ (a[j] >> k)` (`int`) and
/// `(a[j] << k) ^ (a[j] >>> k)` (`long`) with the count register built by
/// `count_load_code` from every masking corner of `k`.
#[test]
fn a_variable_shift_step_masks_the_count_like_java() {
    let h = cratonvm_types::HEADER_SIZE;
    for kind in [VecKind::Int, VecKind::Long] {
        let eb = usize::from(kind.elem_bytes());
        let lanes = kind.lanes() as usize;
        let a: Vec<i64> = (0..lanes as i64)
            .map(|i| i.wrapping_mul(0x1357_9BDF_2468_ACE1) ^ i64::MIN)
            .collect();
        let bytes: Vec<u8> = a
            .iter()
            .flat_map(|v| v.to_le_bytes()[..eb].to_vec())
            .collect();
        let second = if kind == VecKind::Int {
            VShift::Sar
        } else {
            VShift::Shr
        };
        let mut plan = plan_of(
            kind,
            VExpr::Bin(
                VOp::Xor,
                Box::new(VExpr::ShiftBy(VShift::Shl, 0, Box::new(VExpr::Load(1)))),
                Box::new(VExpr::ShiftBy(second, 0, Box::new(VExpr::Load(1)))),
            ),
            2,
            0,
        );
        plan.shift_counts = vec![30];
        let regs = StepRegs {
            temps: &[0, 1, 2],
            inv_regs: &[3],
            acc: None,
            nan: None,
        };
        for k in [0i32, 1, 7, 31, 32, 33, 63, 64, 65, -1, i32::MIN, i32::MAX] {
            // The harness keeps its argument block in R11: PUSH R11 ;
            // MOV R11, imm64 ; the count load ; POP R11.
            let mut body = vec![0x41, 0x53, 0x49, 0xBB];
            body.extend_from_slice(&i64::from(k).to_le_bytes());
            body.extend(count_load_code(kind, 3));
            body.extend_from_slice(&[0x41, 0x5B]);
            body.extend(step_code(&plan, &regs, disp0()).expect("two temporaries"));
            let mut c = fake_array(eb, &[0u8; 32]);
            let a_arr = fake_array(eb, &bytes);
            let mut args = [
                c.as_mut_ptr() as u64,
                a_arr.as_ptr() as u64,
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            if run_on_host(&body, &mut args).is_none() {
                return;
            }
            for (i, &x) in a.iter().enumerate() {
                let want: i64 = if kind == VecKind::Int {
                    let (x, s) = (x as i32, (k & 31) as u32);
                    i64::from((x << s) ^ (x >> s))
                } else {
                    let s = (k & 63) as u32;
                    (x << s) ^ ((x as u64) >> s) as i64
                };
                let mut w = [0u8; 8];
                w[..eb].copy_from_slice(&c[h + i * eb..h + (i + 1) * eb]);
                let got = if kind == VecKind::Int {
                    i64::from(i32::from_le_bytes(w[..4].try_into().unwrap()))
                } else {
                    i64::from_le_bytes(w)
                };
                assert_eq!(got, want, "{kind:?} k={k} lane {i}");
            }
        }
    }
}

/// VEC21-2's narrowing store, executed: two steps of `c[j] = (T) a[j]`
/// from `int` lanes into `byte`, `short` and `char` arrays, against Java's
/// truncation, and nothing written past the sixteen elements.
#[test]
fn a_narrowing_store_truncates_like_xastore() {
    let h = cratonvm_types::HEADER_SIZE;
    let a: Vec<i32> = (0..16i32)
        .map(|i| i.wrapping_mul(0x0765_4321).wrapping_sub(0x7FFF_0000) ^ (i << 7))
        .collect();
    let a_bytes: Vec<u8> = a.iter().flat_map(|v| v.to_le_bytes()).collect();
    for dst in [MemKind::Byte, MemKind::Short, MemKind::Char] {
        let w = usize::from(mem_bytes(dst));
        let mut c = fake_array(w, &vec![0u8; 16 * w]);
        let a_arr = fake_array(4, &a_bytes);
        let mut plan = plan_of(VecKind::Int, VExpr::Load(1), 2, 0);
        plan.base_kinds[0] = dst;
        let mask = narrow_store_mask(&plan).expect("a narrowing plan");
        let regs = StepRegs {
            temps: &[0, 1],
            inv_regs: &[2],
            acc: None,
            nan: None,
        };
        // The harness keeps its argument block in R11.
        let mut body = vec![0x41, 0x53, 0x49, 0xBB]; // PUSH R11 ; MOV R11, imm64
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend(narrow_mask_load_code(2));
        body.extend_from_slice(&[0x41, 0x5B]); // POP R11
        body.extend(step_code(&plan, &regs, disp0()).expect("two temporaries"));
        body.extend(step_code(&plan, &regs, disp0() + 32).expect("two temporaries"));
        let mut args = [
            c.as_mut_ptr() as u64,
            a_arr.as_ptr() as u64,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        if run_on_host(&body, &mut args).is_none() {
            return;
        }
        for (i, &x) in a.iter().enumerate() {
            assert_eq!(
                &c[h + i * w..h + (i + 1) * w],
                &x.to_le_bytes()[..w],
                "{dst:?} lane {i}"
            );
        }
        assert!(
            c[h + 16 * w..].iter().all(|&b| b == 0xA5),
            "{dst:?}: a store past the steps"
        );
    }
    // Not a narrowing plan: same-width, a reduction, `long` lanes.
    assert!(narrow_store_mask(&plan_of(VecKind::Int, VExpr::Load(1), 2, 0)).is_none());
    let mut p = plan_of(VecKind::Long, VExpr::Load(1), 2, 0);
    p.base_kinds[0] = MemKind::Byte;
    assert!(narrow_store_mask(&p).is_none());
}

/// JIT round 24 B1: `c[j + i] = j + i` ([`VExpr::Iota`]) over two steps,
/// `int` and `long` lanes, executed: the init code seeds `j + [0, W)` and
/// every step advances it by `W`; nothing outside the steps' elements is
/// written.
#[test]
fn r24_b1_iota_steps_execute_like_the_induction() {
    for kind in [VecKind::Int, VecKind::Long] {
        let w = usize::from(kind.elem_bytes());
        let lanes = usize::try_from(kind.lanes()).expect("lanes");
        let j = 3usize;
        let total = j + 2 * lanes + 2;
        let mut c = fake_array(w, &vec![0xA5u8; total * w]);
        let mut plan = plan_of(kind, VExpr::Iota, 1, 0);
        plan.iota = true;
        let inv = [2u8, 3];
        assert_eq!(iota_regs(&plan, &inv), Some((2, 3)));
        let regs = StepRegs {
            temps: &[0],
            inv_regs: &inv,
            acc: None,
            nan: None,
        };
        // `run_on_host` keeps its argument block in R11, which the init
        // clobbers: PUSH R11 ... POP R11 around it.
        let mut body = vec![0x41, 0x53];
        body.extend(iota_init_code(kind, 2, 3).expect("iota lanes"));
        body.extend([0x41, 0x5B]);
        let step_bytes = i8::try_from(plan.step_bytes()).expect("step");
        body.extend(step_code(&plan, &regs, disp0()).expect("one temporary"));
        body.extend(step_code(&plan, &regs, disp0() + step_bytes).expect("one temporary"));
        let mut args = [c.as_mut_ptr() as u64, 0, 0, 0, j as u64, 0, 0, 0];
        if run_on_host(&body, &mut args).is_none() {
            return;
        }
        let h = cratonvm_types::HEADER_SIZE;
        for e in 0..total {
            let got = &c[h + e * w..h + (e + 1) * w];
            if (j..j + 2 * lanes).contains(&e) {
                assert_eq!(got, &(e as u64).to_le_bytes()[..w], "{kind:?} element {e}");
            } else {
                assert!(
                    got.iter().all(|&b| b == 0xA5),
                    "{kind:?}: element {e} written"
                );
            }
        }
    }
    // No iota form for a sub-word or FP lane, nor in XMM0 / XMM1.
    assert!(iota_init_code(VecKind::Byte, 2, 3).is_none());
    assert!(iota_init_code(VecKind::Double, 2, 3).is_none());
    assert!(iota_init_code(VecKind::Int, 1, 3).is_none());
}

/// JIT round 24 B1: the init code's encodings, checked against the SDM.
#[test]
fn r24_b1_iota_init_encodings_match_the_sdm() {
    let code = iota_init_code(VecKind::Int, 14, 13).expect("iota lanes");
    // vmovq xmm0, r11
    assert!(code.windows(5).any(|x| x == [0xC4, 0xC1, 0xF9, 0x6E, 0xC3]));
    // vpinsrq xmm0, xmm0, r11, 1
    assert!(code
        .windows(6)
        .any(|x| x == [0xC4, 0xC3, 0xF9, 0x22, 0xC3, 0x01]));
    // vinserti128 ymm14, ymm0, xmm1, 1
    assert!(code
        .windows(6)
        .any(|x| x == [0xC4, 0x63, 0x7D, 0x38, 0xF1, 0x01]));
    // vmovq xmm0, rdx
    assert!(code.windows(5).any(|x| x == [0xC4, 0xE1, 0xF9, 0x6E, 0xC2]));
}

/// JIT round 24 B1: `for (k = 0; k < n; k++) s.setAtIndex(LE_INT, k, v + k)`
/// as the FFM expansion leaves it once its guards are out of the loop:
/// `ScopedPut(seg, scope, base, start + i2l(k) * 4, v + k, le)`. Returns
/// the graph and `[put, start, length, base]`.
fn r24_b1_scoped_put_graph(
    le: i64,
    proven: bool,
    value_reads_base: bool,
) -> (Graph, [NodeId; 4]) {
    use crate::ir::UnboxOp as U;
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let seg = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let v = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let field = |g: &mut Graph, op: U, ty: IrType| {
        g.add(
            Op::Unbox { op, class_id: 804 },
            ty,
            vec![ctrl, mem, seg],
            Some(2),
        )
    };
    let length = field(&mut g, U::FieldLong(0), IrType::Long);
    let start_off = field(&mut g, U::FieldLong(3), IrType::Long);
    let base = field(&mut g, U::FieldRef(2), IrType::Ref);
    let scope = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let merge = g.add(Op::Merge, IrType::Control, vec![ctrl, ctrl], Some(2));
    let mphi = g.add(Op::Phi, IrType::Memory, vec![merge, mem, mem], Some(2));
    let iv = g.add(Op::Phi, IrType::Int, vec![merge, zero, zero], Some(2));
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, n], Some(4));
    let iff = g.add(Op::If, IrType::Control, vec![merge, cmp], Some(4));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(4));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(4));
    let i2l = g.add(Op::I2L, IrType::Long, vec![iv], Some(12));
    let four = g.add(Op::Const(4), IrType::Long, vec![], None);
    let mul = g.add(Op::Mul, IrType::Long, vec![i2l, four], Some(16));
    let addr = g.add(Op::Add, IrType::Long, vec![start_off, mul], Some(16));
    let val = if value_reads_base {
        let elem = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![body, mphi, base, iv],
            Some(15),
        );
        g.add(Op::Add, IrType::Int, vec![elem, iv], Some(15))
    } else {
        g.add(Op::Add, IrType::Int, vec![v, iv], Some(15))
    };
    let order = g.add(Op::Const(le), IrType::Int, vec![], None);
    let put = g.add(
        Op::Unbox {
            op: U::ScopedPut {
                width: 4,
                ordered: true,
            },
            class_id: 0,
        },
        IrType::Int,
        vec![body, mphi, seg, scope, base, addr, val, order],
        Some(16),
    );
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(21));
    g.nodes[merge as usize].inputs[1] = body;
    g.nodes[mphi as usize].inputs[2] = put;
    g.nodes[iv as usize].inputs[2] = next;
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(27));
    if proven {
        g.ffm_proven_heap_accesses.insert(put);
    }
    g.ffm_scoped_length_fields
        .insert(put, (804, U::FieldLong(0)));
    (g, [put, start_off, length, base])
}

fn r24_b1_plan(g: &Graph, scoped_store: bool) -> Result<VecLoopPlan, &'static str> {
    let schedule = crate::ir_schedule::schedule(g);
    let opts = VecOpts {
        bitops: true,
        widen: true,
        scoped_store,
        ..VecOpts::default()
    };
    let (plans, refusals) = plan_vector_loops_with(
        g,
        &schedule,
        &|c| crate::ir_lower::ctrl_block_of(g, &schedule, c),
        opts,
    );
    match (plans.into_iter().next(), refusals.first()) {
        (Some(p), _) => Ok(p),
        (None, Some(&(_, why))) => Err(why),
        (None, None) => Err("no loop"),
    }
}

/// JIT round 24 B1: the heap segment put loop is planned under the
/// switch -- one base (the array), the segment's start and length, an
/// iota value -- and refused without it, big-endian, unproven, or with an
/// element read beside the store.
#[test]
fn r24_b1_a_proven_heap_scoped_put_loop_is_planned() {
    let (g, [put, start, _length, base]) = r24_b1_scoped_put_graph(0, true, false);
    let seg = g.nodes[put as usize].inputs[2];
    let plan = r24_b1_plan(&g, true).expect("planned");
    assert_eq!(plan.kind, VecKind::Int);
    assert_eq!(plan.bases, vec![base]);
    assert_eq!(
        plan.scoped,
        Some(ScopedStore {
            start,
            recv: seg,
            length_field: (804, crate::ir::UnboxOp::FieldLong(0)),
            native: false,
            session: NO_NODE,
        })
    );
    assert!(plan.iota);
    assert_eq!(
        plan.value,
        VExpr::Bin(VOp::Add, Box::new(VExpr::Inv(0)), Box::new(VExpr::Iota))
    );
    assert!(r24_b1_plan(&g, false).is_err(), "switch off");
    let (be, _) = r24_b1_scoped_put_graph(1, true, false);
    assert_eq!(
        r24_b1_plan(&be, true).err(),
        Some("a big-endian scoped store")
    );
    let (unproven, _) = r24_b1_scoped_put_graph(0, false, false);
    assert_eq!(
        r24_b1_plan(&unproven, true).err(),
        Some("a scoped store not proven in bounds")
    );
    let (reads, _) = r24_b1_scoped_put_graph(0, true, true);
    assert_eq!(
        r24_b1_plan(&reads, true).err(),
        Some("a scoped store loop that reads an array")
    );
}

/// JIT round 24 B1: the NATIVE segment put loop -- a literal-null base,
/// the session the loop-entry pre-check's result -- is planned with the
/// pre-check named (the pre-loop tests it reads null), and refused when
/// the session is a plain read.
#[test]
fn r24_b1_a_native_scoped_put_loop_is_planned_with_its_session_precheck() {
    use crate::ir::UnboxOp as U;
    let (mut g, [put, start, _length, _]) = r24_b1_scoped_put_graph(0, false, false);
    let (ctrl, mem, seg) = (1, 2, 3);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let scope_read = g.add(
        Op::Unbox {
            op: U::FieldRef(1),
            class_id: 804,
        },
        IrType::Ref,
        vec![ctrl, mem, seg],
        Some(2),
    );
    let precheck = g.add(
        Op::Unbox {
            op: U::SessionPrecheck,
            class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, scope_read],
        Some(2),
    );
    g.nodes[put as usize].inputs[4] = null;
    g.nodes[put as usize].inputs[3] = precheck;
    let plan = r24_b1_plan(&g, true).expect("planned");
    assert_eq!(plan.bases, vec![null]);
    assert_eq!(
        plan.scoped,
        Some(ScopedStore {
            start,
            recv: seg,
            length_field: (804, U::FieldLong(0)),
            native: true,
            session: precheck,
        })
    );
    // B1 + B2: a cached put reads `k * width` again as its cache `delta`
    // input; that use is its own, so it is still planned (a first build
    // refused it as "a scoped address read besides its store").
    let addr = g.nodes[put as usize].inputs[5];
    let mul = g.nodes[addr as usize]
        .inputs
        .iter()
        .copied()
        .find(|&x| g.nodes[x as usize].op == Op::Mul)
        .expect("k * width");
    for _ in 0..3 {
        let v = g.add(Op::Const(0), IrType::Long, vec![], None);
        g.push_input(put, v);
    }
    g.push_input(put, mul);
    assert_eq!(
        r24_b1_plan(&g, true).err(),
        Some("a scoped address read besides its store"),
        "an unmarked extra reader still refuses"
    );
    g.ffm_arena_cached.insert(put);
    assert!(r24_b1_plan(&g, true).is_ok(), "the cached put is planned");
    g.nodes[put as usize].inputs[3] = scope_read;
    assert_eq!(
        r24_b1_plan(&g, true).err(),
        Some("a native scoped store whose session is not pre-checked")
    );
}

/// The pre-check never leaves the body, so it is no pre-header trap.
#[test]
fn r24_b1_the_session_precheck_cannot_raise() {
    assert!(!crate::ir::may_raise(&Op::Unbox {
        op: crate::ir::UnboxOp::SessionPrecheck,
        class_id: 0,
    }));
    assert!(crate::ir::may_raise(&Op::Unbox {
        op: crate::ir::UnboxOp::FieldLong(0),
        class_id: 804,
    }));
}
