use super::*;
use crate::x64::{R10, R11, R12, R13, R9, RAX, RBP, RBX, RCX, RSI, RSP};

/// A fresh single-pass `Compiler` whose hand-written emitters can be
/// driven directly. Same construction as `isel`'s reference harness.
///
/// `Compiler::new` has already emitted a method prologue; callers that
/// execute what they emit must enter at the `buf.pos()` they recorded.
pub(in crate::x64) fn test_compiler() -> Compiler {
    let alloc = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    Compiler::new(
        "emit-unit".to_string(),
        crate::ExecutableBuffer::new(1 << 16).expect("test executable buffer"),
        0,
        0,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc,
        false,
        cratonvm_jit_api::JitRuntimeHelpers::default(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    )
}

/// The bytes one emitter call appends.
fn bytes(c: &mut Compiler, f: impl FnOnce(&mut Compiler)) -> Vec<u8> {
    let start = c.buf.pos();
    f(c);
    c.buf.as_slice()[start..].to_vec()
}

/// An R12/RSP base gets the index-free SIB byte in every `*_disp32`
/// emitter; every other base keeps exactly the bytes it always had.
#[test]
fn disp32_family_emits_the_sib_byte_for_rsp_class_bases() {
    let mut c = test_compiler();
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_r64_mem_disp32(RAX, R12, 8)),
        vec![0x49, 0x8B, 0x84, 0x24, 0x08, 0x00, 0x00, 0x00],
        "MOV RAX, [R12 + 8]"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_mem_disp32_r64(RSP, RCX, 16)),
        vec![0x48, 0x89, 0x8C, 0x24, 0x10, 0x00, 0x00, 0x00],
        "MOV [RSP + 16], RCX"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_dword_mem_disp32_imm32(R12, 4, 7)),
        vec![0x41, 0xC7, 0x84, 0x24, 0x04, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00],
        "MOV DWORD [R12 + 4], 7 — the SIB sits between ModRM and disp32"
    );
    // Unchanged for a non-SIB base.
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_r64_mem_disp32(RAX, RBX, 8)),
        vec![0x48, 0x8B, 0x83, 0x08, 0x00, 0x00, 0x00],
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_r64_mem_disp32(RAX, R13, 0)),
        vec![0x49, 0x8B, 0x85, 0x00, 0x00, 0x00, 0x00],
    );
    assert!(!c.buf.overflowed());
}

/// Byte registers 4..=7 need a bare REX or they are AH/CH/DH/BH; 8..=15
/// need REX.R / REX.B. AL..BL keep their REX-less encodings.
#[test]
fn byte_register_forms_encode_every_register() {
    let mut c = test_compiler();
    assert_eq!(
        bytes(&mut c, |c| c.emit_test_r8_imm8(RCX, 1)),
        vec![0xF6, 0xC1, 0x01]
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_test_r8_imm8(RSI, 1)),
        vec![0x40, 0xF6, 0xC6, 0x01],
        "SIL, not DH"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_test_r8_imm8(R9, 1)),
        vec![0x41, 0xF6, 0xC1, 0x01],
        "R9B, not CL"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_cmp_r8_mem8(RCX, R11, 0)),
        vec![0x41, 0x3A, 0x0B],
        "the one existing caller is unchanged"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_cmp_r8_mem8(RSI, RAX, 0)),
        vec![0x40, 0x3A, 0x30],
        "CMP SIL, [RAX]"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_cmp_r8_mem8(R9, RAX, 0)),
        vec![0x44, 0x3A, 0x08],
        "CMP R9B, [RAX]"
    );
    assert!(!c.buf.overflowed());
}

/// `[base + index]` byte store: RBP/R13 bases take `mod=01` + `disp8(0)`
/// (a `mod=00` SIB with base=101 is "no base, disp32"), and an RSP index —
/// which SIB cannot express — refuses the method.
#[test]
fn indexed_byte_store_handles_both_sib_special_cases() {
    let mut c = test_compiler();
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_mem8_indexed_imm8(R11, RCX, 1)),
        vec![0x41, 0xC6, 0x04, 0x0B, 0x01],
        "the card-mark store is unchanged"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_mem8_indexed_imm8(RBP, RCX, 1)),
        vec![0xC6, 0x44, 0x0D, 0x00, 0x01],
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_mem8_indexed_imm8(R13, RCX, 1)),
        vec![0x41, 0xC6, 0x44, 0x0D, 0x00, 0x01],
    );
    assert!(!c.buf.overflowed());
    c.emit_mov_mem8_indexed_imm8(RAX, RSP, 1);
    assert!(
        c.buf.overflowed(),
        "an RSP index must refuse, not store to [base]"
    );
}

/// A displacement past `i8` refuses the method instead of wrapping.
#[test]
fn add_mem_disp8_refuses_a_wide_displacement() {
    let mut c = test_compiler();
    assert_eq!(
        bytes(&mut c, |c| c.emit_add_mem_disp8_imm8(R10, 8, 1)),
        vec![0x49, 0x83, 0x42, 0x08, 0x01],
    );
    assert!(!c.buf.overflowed());
    c.emit_add_mem_disp8_imm8(R10, 128, 1);
    assert!(
        c.buf.overflowed(),
        "disp 128 would have been encoded as -128"
    );
}

/// The register-direct VEX emitters take the 3-byte prefix when the
/// `r/m` register is >= 8 (the 2-byte form has no VEX.B), and are
/// byte-identical to the old 2-byte form otherwise.
#[test]
fn vex_reg_reg_forms_reach_the_high_registers() {
    let mut c = test_compiler();
    assert_eq!(
        bytes(&mut c, |c| c.emit_vpxor_ymm(0, 0, 0)),
        vec![0xC5, 0xFD, 0xEF, 0xC0],
        "the existing SIMD call site is unchanged"
    );
    // VPXOR YMM0, YMM0, YMM9: C4 (R X !B, map 0F) (W0 vvvv=~0 L=1 pp=66).
    assert_eq!(
        bytes(&mut c, |c| c.emit_vpxor_ymm(0, 0, 9)),
        vec![0xC4, 0xC1, 0x7D, 0xEF, 0xC1],
    );
    assert!(!c.buf.overflowed());
}

/// EVEX 512-bit encodings of the AVX-512 sieve scan, checked byte-for-byte
/// against Intel SDM Vol 2A, including the extended-register bits
/// (EVEX.R/X/B/R'/V') that only appear with r8-r15 / zmm16-zmm31.
#[test]
fn evex_512_instructions_encode_exact_byte_sequences() {
    let mut c = test_compiler();

    // VPXORD ZMM0, ZMM0, ZMM0: 62 f1 7d 48 ef c0
    assert_eq!(
        bytes(&mut c, |c| c.emit_vpxord_zmm(0, 0, 0)),
        vec![0x62, 0xF1, 0x7D, 0x48, 0xEF, 0xC0],
        "VPXORD ZMM0, ZMM0, ZMM0"
    );

    // VPXORD ZMM1, ZMM2, ZMM3: 62 f1 6d 48 ef cb
    assert_eq!(
        bytes(&mut c, |c| c.emit_vpxord_zmm(1, 2, 3)),
        vec![0x62, 0xF1, 0x6D, 0x48, 0xEF, 0xCB],
        "VPXORD ZMM1, ZMM2, ZMM3"
    );

    // VPXORD ZMM17, ZMM18, ZMM19: 62 a1 6d 40 ef cb (R'=0, X=0, V'=0)
    assert_eq!(
        bytes(&mut c, |c| c.emit_vpxord_zmm(17, 18, 19)),
        vec![0x62, 0xA1, 0x6D, 0x40, 0xEF, 0xCB],
        "VPXORD ZMM17, ZMM18, ZMM19"
    );

    // KMOVQ RAX, K1: c4 e1 fb 93 c1
    assert_eq!(
        bytes(&mut c, |c| c.emit_kmovq_gpr_k(RAX, 1)),
        vec![0xC4, 0xE1, 0xFB, 0x93, 0xC1],
        "KMOVQ RAX, K1"
    );

    // KMOVQ R9, K2: c4 61 fb 93 ca (VEX.R=0 for r9)
    assert_eq!(
        bytes(&mut c, |c| c.emit_kmovq_gpr_k(R9, 2)),
        vec![0xC4, 0x61, 0xFB, 0x93, 0xCA],
        "KMOVQ R9, K2"
    );

    // VPCMPB K1, ZMM0, ZMM1, 0: 62 f3 7d 48 3f c9 00
    assert_eq!(
        bytes(&mut c, |c| c.emit_vpcmpb_k_zmm_zmm(1, 0, 1, 0)),
        vec![0x62, 0xF3, 0x7D, 0x48, 0x3F, 0xC9, 0x00],
        "VPCMPB K1, ZMM0, ZMM1, 0 (EQ)"
    );

    // VMOVDQU8 ZMM0, [RAX + R10]: 62 b1 7f 48 6f 04 10
    assert_eq!(
        bytes(&mut c, |c| c.emit_vmovdqu8_load_zmm_indexed(0, RAX, R10)),
        vec![0x62, 0xB1, 0x7F, 0x48, 0x6F, 0x04, 0x10],
        "VMOVDQU8 ZMM0, [RAX + R10]"
    );

    // VMOVDQU8 ZMM9, [R8 + RCX]: 62 51 7f 48 6f 0c 08 (EVEX.R=0, EVEX.B=0)
    assert_eq!(
        bytes(&mut c, |c| c.emit_vmovdqu8_load_zmm_indexed(9, R8, RCX)),
        vec![0x62, 0x51, 0x7F, 0x48, 0x6F, 0x0C, 0x08],
        "VMOVDQU8 ZMM9, [R8 + RCX]"
    );

    assert!(!c.buf.overflowed());
}

/// `mod=00` with a base of `101` is "disp32, no base", so RBP/R13 cannot
/// be the base of the displacement-free indexed load; the encoder refuses
/// instead of emitting a load from an absolute address.
#[test]
#[should_panic(expected = "RBP/R13")]
fn evex_indexed_load_refuses_a_base_that_needs_a_displacement() {
    let mut c = test_compiler();
    c.emit_vmovdqu8_load_zmm_indexed(0, 13, R10);
}

/// Non-negative constants take the zero-extending `MOV r32, imm32`
/// (`mov-imm-non-negative-uses-sign-extending-c7-form-20260918.md`);
/// negative ones keep the sign-extending `REX.W C7 /0`, wider ones MOVABS,
/// zero keeps `XOR`, and the IC form stays ten bytes.
#[test]
fn non_negative_immediates_take_the_zero_extending_form() {
    let mut c = test_compiler();
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm32_sx(R9, 1)),
        vec![0x41, 0xB9, 0x01, 0x00, 0x00, 0x00],
        "MOV R9D, 1"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm32_sx(RCX, i32::MAX)),
        vec![0xB9, 0xFF, 0xFF, 0xFF, 0x7F],
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm32_sx(RCX, -1)),
        vec![0x48, 0xC7, 0xC1, 0xFF, 0xFF, 0xFF, 0xFF],
        "a negative value still sign-extends"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm32_sx(R10, i32::MIN)),
        vec![0x49, 0xC7, 0xC2, 0x00, 0x00, 0x00, 0x80],
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm64(RAX, 0xFFFF_FFFF)),
        vec![0xB8, 0xFF, 0xFF, 0xFF, 0xFF],
        "MOV EAX, 0xFFFFFFFF zero-extends to 0x0000_0000_FFFF_FFFF"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm64(R11, i64::from(i32::MAX) + 1)),
        vec![0x41, 0xBB, 0x00, 0x00, 0x00, 0x80],
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm64(RAX, 1 << 32)),
        vec![0x48, 0xB8, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00],
        "past u32 needs MOVABS"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm64(RAX, i64::from(i32::MIN) - 1)).len(),
        10,
        "below i32::MIN needs MOVABS"
    );
    assert_eq!(
        bytes(&mut c, |c| c.emit_mov_imm64(RAX, 0)),
        vec![0x31, 0xC0]
    );
    assert_eq!(bytes(&mut c, |c| c.emit_mov_imm64_full(RAX, 1)).len(), 10);
    assert!(!c.buf.overflowed());
}

/// t1gate: a rebased RIP displacement is exact in reach and refused, not
/// truncated, one byte past either edge of `disp32`.
#[test]
fn t1gate_rebase_disp32_is_exact_in_reach_and_refuses_past_both_edges() {
    let next: usize = 0x5000_0000_0000;
    let r = |delta: i64| rebase_disp32(next as i64 + delta, next);
    assert_eq!(r(0), Some(0));
    assert_eq!(r(1234), Some(1234));
    assert_eq!(r(-1234), Some(-1234));
    assert_eq!(r(i64::from(i32::MAX)), Some(i32::MAX));
    assert_eq!(r(i64::from(i32::MIN)), Some(i32::MIN));
    assert_eq!(r(i64::from(i32::MAX) + 1), None);
    assert_eq!(r(i64::from(i32::MIN) - 1), None);
    // The 64-bit-far case the truncating cast turned into a small value.
    assert_eq!(r((1i64 << 32) + 8), None);
}

/// t1gate: the failure the unroller used to truncate. A target exactly at
/// the original site's `i32::MIN` reach is out of reach of a copy one loop
/// body higher; the old `delta as i32` wrapped it to a large positive value.
#[test]
fn t1gate_an_unrolled_copy_one_body_past_the_low_edge_is_refused() {
    let body_len: usize = 96;
    let original_next_pc: usize = 0x6000_0000_0000;
    let target = original_next_pc as i64 + i64::from(i32::MIN);
    let copy_next_pc = original_next_pc + body_len;
    assert_eq!(
        rebase_disp32(target, original_next_pc),
        Some(i32::MIN),
        "the original site is in reach"
    );
    assert_eq!(
        rebase_disp32(target, copy_next_pc),
        None,
        "the copy, one body higher, is not"
    );
    // A target above the code gets closer as the copy moves up: still fine.
    let above = original_next_pc as i64 + i64::from(i32::MAX);
    let shrunk = i32::MAX - i32::try_from(body_len).unwrap_or(0);
    assert_eq!(rebase_disp32(above, copy_next_pc), Some(shrunk));
}
