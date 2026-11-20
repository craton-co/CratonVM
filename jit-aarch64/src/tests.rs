use super::*;

// -- Reg::from_u8 / FpReg::from_u8 (C11) --------------------------------

#[test]
fn reg_from_u8_round_trips_all_valid_encodings() {
    // X0..X30 round-trip. 31 is not a general-purpose register after A8 --
    // it is `RegSp::Sp` or `RegZr::Zr` depending on the field -- so it is
    // the one value in 0..=31 `Reg::from_u8` must refuse.
    for n in 0..=30u8 {
        let r = Reg::from_u8(n).expect("0..=30 must round-trip");
        assert_eq!(r as u8, n, "Reg::from_u8({n}) must encode back to {n}");
    }
    assert_eq!(Reg::from_u8(31), None);
    assert_eq!(RegSp::from_u8(31).map(RegSp::enc), Some(31));
}

#[test]
fn reg_from_u8_rejects_out_of_range() {
    for n in [32u8, 40, 64, 100, 200, 255] {
        assert!(Reg::from_u8(n).is_none(), "Reg::from_u8({n}) must be None");
    }
}

#[test]
fn fpreg_from_u8_round_trips_all_valid_encodings() {
    for n in 0..=31u8 {
        let r = FpReg::from_u8(n).expect("0..=31 must round-trip");
        assert_eq!(r as u8, n, "FpReg::from_u8({n}) must encode back to {n}");
    }
}

#[test]
fn fpreg_from_u8_rejects_out_of_range() {
    for n in [32u8, 40, 64, 100, 200, 255] {
        assert!(
            FpReg::from_u8(n).is_none(),
            "FpReg::from_u8({n}) must be None"
        );
    }
}

/// Helper: read the last emitted 32-bit instruction word.
fn last_inst(e: &Aarch64Emitter) -> u32 {
    let c = e.code();
    let len = c.len();
    u32::from_le_bytes([c[len - 4], c[len - 3], c[len - 2], c[len - 1]])
}

/// Helper: read instruction at a specific byte offset.
fn inst_at(e: &Aarch64Emitter, off: usize) -> u32 {
    let c = e.code();
    u32::from_le_bytes([c[off], c[off + 1], c[off + 2], c[off + 3]])
}

// -- Arithmetic ---------------------------------------------------------

#[test]
fn test_add_x0_x1_x2() {
    let mut e = Aarch64Emitter::new();
    e.add(Reg::X0, Reg::X1, Reg::X2);
    // ADD X0, X1, X2: sf=1, opc=00, 01011 00 0 Rm=00010 000000 Rn=00001 Rd=00000
    // 1 00 01011 00 0 00010 000000 00001 00000
    // = 0x8B020020
    assert_eq!(last_inst(&e), 0x8B02_0020);
}

#[test]
fn test_add_w_regs() {
    let mut e = Aarch64Emitter::new();
    e.add_w(Reg::X3, Reg::X4, Reg::X5);
    // sf=0: 0 00 01011 00 0 00101 000000 00100 00011
    // = 0x0B050083
    assert_eq!(last_inst(&e), 0x0B05_0083);
}

#[test]
fn test_sub_x0_x1_x2() {
    let mut e = Aarch64Emitter::new();
    e.sub(Reg::X0, Reg::X1, Reg::X2);
    // SUB X0, X1, X2: sf=1, opc=10, ...
    // = 0xCB020020
    assert_eq!(last_inst(&e), 0xCB02_0020);
}

#[test]
fn test_mul_x0_x1_x2() {
    let mut e = Aarch64Emitter::new();
    e.mul(Reg::X0, Reg::X1, Reg::X2);
    // MADD X0, X1, X2, XZR
    // sf=1 00 11011 000 Rm=00010 o0=0 Ra=11111 Rn=00001 Rd=00000
    // 1 00 11011 000 00010 0 11111 00001 00000
    // = 0x9B027C20
    assert_eq!(last_inst(&e), 0x9B02_7C20);
}

#[test]
fn test_sdiv() {
    let mut e = Aarch64Emitter::new();
    e.sdiv(Reg::X0, Reg::X1, Reg::X2);
    // 1 0 0 11010110 00010 000011 00001 00000
    // = 0x9AC20C20
    assert_eq!(last_inst(&e), 0x9AC2_0C20);
}

#[test]
fn test_add_imm() {
    let mut e = Aarch64Emitter::new();
    e.add_imm(SP, SP, 16, false);
    // sf=1 op=0 S=0 100010 sh=0 imm12=000000010000 Rn=11111 Rd=11111
    // 1 0 0 100010 0 000000010000 11111 11111
    // = 0x910043FF
    assert_eq!(last_inst(&e), 0x9100_43FF);
}

// -- Logic --------------------------------------------------------------

#[test]
fn test_and_orr_eor() {
    let mut e = Aarch64Emitter::new();
    e.and(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x8A02_0020);

    e.orr(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0xAA02_0020);

    e.eor(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0xCA02_0020);
}

// -- Move ---------------------------------------------------------------

#[test]
fn test_movz() {
    let mut e = Aarch64Emitter::new();
    e.movz(Reg::X0, 0x1234, 0);
    // sf=1 10 100101 00 0001001000110100 00000
    // = 0xD2824680
    assert_eq!(last_inst(&e), 0xD282_4680);
}

#[test]
fn test_movk() {
    let mut e = Aarch64Emitter::new();
    e.movk(Reg::X0, 0xABCD, 16);
    // sf=1 11 100101 01 imm16 Rd
    // = 0xF2A579A0 (hw=1)
    let inst = last_inst(&e);
    // Verify opc bits: 11 = MOVK
    assert_eq!((inst >> 29) & 0x3, 0b11);
    // Verify hw = 1 (shift=16)
    assert_eq!((inst >> 21) & 0x3, 1);
    // Verify Rd = 0
    assert_eq!(inst & 0x1F, 0);
}

#[test]
fn test_mov_imm64_simple() {
    let mut e = Aarch64Emitter::new();
    e.mov_imm64(Reg::X0, 42);
    // Should be a single MOVZ X0, #42, LSL #0
    assert_eq!(e.code().len(), 4); // single instruction
    let inst = inst_at(&e, 0);
    // Check it's MOVZ with imm=42
    assert_eq!((inst >> 29) & 0x3, 0b10); // MOVZ opc
    assert_eq!((inst >> 5) & 0xFFFF, 42);
    assert_eq!(inst & 0x1F, 0); // Rd = X0
}

#[test]
fn test_mov_imm64_two_chunks() {
    let mut e = Aarch64Emitter::new();
    e.mov_imm64(Reg::X1, 0x0000_0000_DEAD_BEEF);
    // chunk0 = 0xBEEF, chunk1 = 0xDEAD, chunk2 = 0, chunk3 = 0
    // Should be: MOVZ X1, #0xBEEF, LSL#0 + MOVK X1, #0xDEAD, LSL#16
    assert_eq!(e.code().len(), 8); // two instructions

    let i0 = inst_at(&e, 0);
    assert_eq!((i0 >> 29) & 0x3, 0b10); // MOVZ
    assert_eq!((i0 >> 5) & 0xFFFF, 0xBEEF);
    assert_eq!(i0 & 0x1F, 1); // X1

    let i1 = inst_at(&e, 4);
    assert_eq!((i1 >> 29) & 0x3, 0b11); // MOVK
    assert_eq!((i1 >> 5) & 0xFFFF, 0xDEAD);
}

#[test]
fn test_mov_imm64_all_ones() {
    let mut e = Aarch64Emitter::new();
    e.mov_imm64(Reg::X0, u64::MAX);
    // All chunks 0xFFFF, should use MOVN X0, #0, LSL#0
    assert_eq!(e.code().len(), 4);
    let inst = inst_at(&e, 0);
    assert_eq!((inst >> 29) & 0x3, 0b00); // MOVN opc
}

#[test]
fn test_mov_imm64_full_pattern() {
    let mut e = Aarch64Emitter::new();
    e.mov_imm64(Reg::X2, 0x1111_2222_3333_4444);
    // All four chunks non-zero: needs MOVZ + 3x MOVK = 16 bytes
    assert_eq!(e.code().len(), 16);
}

// -- Branch -------------------------------------------------------------

#[test]
fn test_b_forward() {
    let mut e = Aarch64Emitter::new();
    e.b(0x100);
    let inst = last_inst(&e);
    // B: 000101 imm26
    assert_eq!((inst >> 26) & 0x3F, 0b000101);
    // imm26 = 0x100 / 4 = 0x40
    assert_eq!(inst & 0x03FF_FFFF, 0x40);
}

#[test]
fn test_bl() {
    let mut e = Aarch64Emitter::new();
    e.bl(0x200);
    let inst = last_inst(&e);
    assert_eq!((inst >> 26) & 0x3F, 0b100101);
    assert_eq!(inst & 0x03FF_FFFF, 0x80);
}

#[test]
fn test_br_blr_ret() {
    let mut e = Aarch64Emitter::new();
    e.br(Reg::X16);
    assert_eq!(last_inst(&e), 0xD61F_0200);

    e.blr(Reg::X8);
    assert_eq!(last_inst(&e), 0xD63F_0100);

    e.ret_lr();
    assert_eq!(last_inst(&e), 0xD65F_03C0);
}

#[test]
fn test_b_cond() {
    let mut e = Aarch64Emitter::new();
    e.b_cond(Cond::EQ, 0x10);
    let inst = last_inst(&e);
    // 0101010 0 imm19 0 cond
    assert_eq!(inst & 0xF, Cond::EQ.enc());
    // imm19 = 0x10/4 = 4, shifted left by 5
    assert_eq!((inst >> 5) & 0x7FFFF, 4);
}

#[test]
fn test_cbz_cbnz() {
    let mut e = Aarch64Emitter::new();
    e.cbz(Reg::X5, 0x20);
    let inst = last_inst(&e);
    assert_eq!(inst & 0x1F, 5); // Rt = X5
    assert_eq!((inst >> 24) & 0xFF, 0xB4); // CBZ 64-bit prefix

    e.cbnz(Reg::X3, 0x10);
    let inst = last_inst(&e);
    assert_eq!(inst & 0x1F, 3);
    assert_eq!((inst >> 24) & 0xFF, 0xB5); // CBNZ 64-bit prefix
}

#[test]
fn test_tbz_tbnz() {
    let mut e = Aarch64Emitter::new();
    // TBZ X0, #5, +8
    e.tbz(Reg::X0, 5, 8);
    let inst = last_inst(&e);
    assert_eq!(inst & 0x1F, 0); // Rt = X0
                                // bit 5 in b40 field
    assert_eq!((inst >> 19) & 0x1F, 5);
    // imm14 = 8/4 = 2
    assert_eq!((inst >> 5) & 0x3FFF, 2);

    // TBNZ X1, #63, +0xC
    e.tbnz(Reg::X1, 63, 0xC);
    let inst = last_inst(&e);
    // b5=1 (bit 31), b40=31
    assert_eq!((inst >> 31) & 1, 1);
    assert_eq!((inst >> 19) & 0x1F, 31);
}

// -- Load / Store -------------------------------------------------------

#[test]
fn test_ldr_str_imm() {
    let mut e = Aarch64Emitter::new();
    // LDR X0, [X1, #8]
    assert!(e.ldr_imm(Reg::X0, Reg::X1, 8));
    let inst = last_inst(&e);
    // size=11, V=0, opc=01 → top byte 0xF9
    assert_eq!((inst >> 22) & 0x3FF, 0b11_111_0_01_01);
    // imm12 = 8/8 = 1
    assert_eq!((inst >> 10) & 0xFFF, 1);
    assert_eq!((inst >> 5) & 0x1F, 1); // Rn = X1
    assert_eq!(inst & 0x1F, 0); // Rt = X0

    // STR X2, [SP, #16]
    assert!(e.str_imm(Reg::X2, SP, 16));
    let inst = last_inst(&e);
    assert_eq!((inst >> 10) & 0xFFF, 2); // 16/8=2
    assert_eq!((inst >> 5) & 0x1F, 31); // SP
    assert_eq!(inst & 0x1F, 2); // X2
}

#[test]
fn test_ldur_stur_unscaled() {
    // Bug-fix (ARM64 BUG #1): unscaled, non-writeback offset access.
    // LDUR X0, [X1, #-8]:
    //   size=11 V=0 opc=01, imm9=0x1F8 (-8), Rn=1, Rt=0, bits11:10=00
    //   = 0xF85F8020
    let mut e = Aarch64Emitter::new();
    e.ldur(Reg::X0, Reg::X1, -8);
    assert_eq!(last_inst(&e), 0xF85F_8020);

    // STUR X2, [SP, #-16]:
    //   size=11 V=0 opc=00, imm9=0x1F0 (-16), Rn=31, Rt=2, bits11:10=00
    //   = 0xF81F03E2
    e.stur(Reg::X2, SP, -16);
    assert_eq!(last_inst(&e), 0xF81F_03E2);

    // Crucially the unscaled form must NOT set the writeback/index bit
    // (bit 10) — that bit being 1 would mutate the base register.
    let inst = last_inst(&e);
    assert_eq!(
        (inst >> 10) & 0x3,
        0b00,
        "LDUR/STUR must be unscaled (bits 11:10 == 00)"
    );
    // And it is distinct from the unsigned-offset form (bits 24 == 0).
    assert_eq!(
        (inst >> 24) & 1,
        0,
        "LDUR/STUR is not the scaled unsigned-offset form"
    );
}

/// Unscaled FP load/store — the encodings the negative-frame-offset FP
/// spill path depends on.
///
/// Each expected word is the GPR LDUR/STUR word from
/// `test_ldur_stur_unscaled` with bit 26 (V) set, which is exactly what
/// the ARM ARM "Load/store register (unscaled immediate)" table says
/// distinguishes the SIMD&FP variant from the general-purpose one.
#[test]
fn test_ldur_stur_fp_unscaled() {
    let mut e = Aarch64Emitter::new();

    // LDUR D0, [X1, #-8] = 0xFC5F8020 (GPR form 0xF85F8020 | V bit)
    e.ldur_fp_d(FpReg::D0, Reg::X1, -8);
    assert_eq!(last_inst(&e), 0xFC5F_8020);

    // STUR D2, [SP, #-16] = 0xFC1F03E2 (GPR form 0xF81F03E2 | V bit)
    e.stur_fp_d(FpReg::D2, SP, -16);
    assert_eq!(last_inst(&e), 0xFC1F_03E2);

    // LDUR S0, [X1, #-4] = 0xBC5FC020
    e.ldur_fp_s(FpReg::D0, Reg::X1, -4);
    assert_eq!(last_inst(&e), 0xBC5F_C020);

    // STUR S3, [X29, #-4] = 0xBC1FC3A3
    e.stur_fp_s(FpReg::D3, Reg::X29, -4);
    assert_eq!(last_inst(&e), 0xBC1F_C3A3);
}

/// The unscaled FP forms must not set the index/writeback bit — if bit 10
/// were 1 the instruction would become pre/post-index and would MUTATE the
/// base register, corrupting FP on every FP spill (the exact shape of
/// ARM64 BUG #1 on the GPR side).
#[test]
fn test_fp_unscaled_has_no_writeback() {
    for i in 0..4u32 {
        let mut e = Aarch64Emitter::new();
        match i {
            0 => e.ldur_fp_d(FpReg::D5, Reg::X29, -32),
            1 => e.stur_fp_d(FpReg::D5, Reg::X29, -32),
            2 => e.ldur_fp_s(FpReg::D5, Reg::X29, -32),
            _ => e.stur_fp_s(FpReg::D5, Reg::X29, -32),
        }
        let inst = last_inst(&e);
        assert_eq!(
            (inst >> 10) & 0x3,
            0b00,
            "FP LDUR/STUR variant {i} must be unscaled/non-writeback"
        );
        assert_eq!(
            (inst >> 26) & 1,
            1,
            "variant {i} must set the V (SIMD&FP) bit"
        );
        assert_eq!((inst >> 24) & 1, 0, "variant {i} is not the scaled form");
    }
}

/// Barrier encodings, checked against the ARM ARM's `CRm` option table.
///
/// These are the primitives any future aarch64 publication/volatile/monitor
/// lowering has to reach for — nothing in the backend emits them today (see
/// `docs/jit/aarch64-parity.md`), so pinning their bytes now is the cheapest
/// way to keep them trustworthy until something does.
#[test]
fn test_barrier_encodings() {
    let mut e = Aarch64Emitter::new();

    e.dsb(0b1111); // DSB SY
    assert_eq!(last_inst(&e), 0xD503_3F9F);
    e.dsb(0b1011); // DSB ISH
    assert_eq!(last_inst(&e), 0xD503_3B9F);

    e.dmb(0b1111); // DMB SY
    assert_eq!(last_inst(&e), 0xD503_3FBF);
    // The barrier encoding is `base | CRm << 8`, so CRm lands in the third
    // hex digit from the right: DMB's base is D50330BF, and ISH (CRm=1011)
    // is D5033BBF, not D50333BF. The two literals below were written with
    // that nibble one position out — an easy transposition, and precisely
    // why these bytes are pinned against the ARM ARM rather than against
    // the emitter that produced them.
    e.dmb(0b1011); // DMB ISH
    assert_eq!(last_inst(&e), 0xD503_3BBF);
    e.dmb(0b1010); // DMB ISHST
    assert_eq!(last_inst(&e), 0xD503_3ABF);

    e.isb(); // ISB SY
    assert_eq!(last_inst(&e), 0xD503_3FDF);
}

/// LDAR/STLR/LDAXR/STLXR/CASAL, pinned against the ARM ARM's
/// "load/store exclusive / ordered" class (C4.1.94 / C6.2):
/// `size 001000 o2 L o1 Rs o0 Rt2 Rn Rt`, Rt2 = 11111, and Rs = 11111
/// wherever the instruction has no status/compare register.
#[test]
fn test_acquire_release_and_exclusive_encodings() {
    let mut e = Aarch64Emitter::new();

    e.ldar(Reg::X0, Reg::X1); // LDAR X0, [X1]
    assert_eq!(last_inst(&e), 0xC8DF_FC20);
    e.ldar_w(Reg::X2, SP); // LDAR W2, [SP]
    assert_eq!(last_inst(&e), 0x88DF_FFE2);
    e.ldarh(Reg::X3, Reg::X4); // LDARH W3, [X4]
    assert_eq!(last_inst(&e), 0x48DF_FC83);
    e.ldarb(Reg::X5, Reg::X6); // LDARB W5, [X6]
    assert_eq!(last_inst(&e), 0x08DF_FCC5);

    e.stlr(Reg::X0, Reg::X1); // STLR X0, [X1]
    assert_eq!(last_inst(&e), 0xC89F_FC20);
    e.stlr_w(XZR, Reg::X9); // STLR WZR, [X9]
    assert_eq!(last_inst(&e), 0x889F_FD3F);
    e.stlrh(Reg::X3, Reg::X4); // STLRH W3, [X4]
    assert_eq!(last_inst(&e), 0x489F_FC83);
    e.stlrb(Reg::X5, Reg::X6); // STLRB W5, [X6]
    assert_eq!(last_inst(&e), 0x089F_FCC5);

    e.ldaxr(Reg::X0, Reg::X1); // LDAXR X0, [X1]
    assert_eq!(last_inst(&e), 0xC85F_FC20);
    e.ldaxr_w(Reg::X0, Reg::X1); // LDAXR W0, [X1]
    assert_eq!(last_inst(&e), 0x885F_FC20);

    assert!(e.stlxr(Reg::X2, Reg::X0, Reg::X1)); // STLXR W2, X0, [X1]
    assert_eq!(last_inst(&e), 0xC802_FC20);
    assert!(e.stlxr_w(Reg::X2, Reg::X0, Reg::X1)); // STLXR W2, W0, [X1]
    assert_eq!(last_inst(&e), 0x8802_FC20);

    e.casal(Reg::X2, Reg::X0, Reg::X1); // CASAL X2, X0, [X1]
    assert_eq!(last_inst(&e), 0xC8E2_FC20);
    e.casal_w(Reg::X2, Reg::X0, Reg::X1); // CASAL W2, W0, [X1]
    assert_eq!(last_inst(&e), 0x88E2_FC20);
}

/// STLXR with its status register equal to the value or the base is
/// CONSTRAINED UNPREDICTABLE; the encoder refuses and emits nothing.
#[test]
fn stlxr_refuses_a_status_register_that_aliases_an_operand() {
    let mut e = Aarch64Emitter::new();
    let before = e.code().len();
    assert!(!e.stlxr(Reg::X0, Reg::X0, Reg::X1), "Ws == Xt");
    assert!(!e.stlxr(Reg::X1, Reg::X0, Reg::X1), "Ws == Xn");
    assert!(!e.stlxr_w(Reg::X0, Reg::X0, Reg::X1), "Ws == Wt");
    assert_eq!(e.code().len(), before, "a refusal emits nothing");
    // SP as the base cannot alias a general-purpose status register.
    assert!(e.stlxr(Reg::X2, Reg::X0, SP));
    assert_eq!(last_inst(&e), 0xC802_FFE0);
}

/// Round 9 wave 10: the narrow plain load/store forms the backend's
/// `MemLoad`/`MemStore` pseudo-ops need beside the existing `LDRB`, `LDR W`
/// and `LDR X`. Words checked against the ARM ARM "load/store register
/// (unsigned immediate)" class; a misaligned or over-wide offset refuses.
#[test]
fn r9w10_narrow_unsigned_offset_load_store_encodings() {
    let mut e = Aarch64Emitter::new();
    assert!(e.ldrh_imm(Reg::X3, Reg::X4, 0)); // LDRH W3, [X4]
    assert_eq!(last_inst(&e), 0x7940_0083);
    assert!(e.ldrh_imm(Reg::X3, Reg::X4, 2)); // LDRH W3, [X4, #2]
    assert_eq!(last_inst(&e), 0x7940_0483);
    assert!(e.strb_imm(Reg::X5, Reg::X6, 7)); // STRB W5, [X6, #7]
    assert_eq!(last_inst(&e), 0x3900_1CC5);
    assert!(e.strb_imm(XZR, SP, 0)); // STRB WZR, [SP]
    assert_eq!(last_inst(&e), 0x3900_03FF);
    assert!(e.strh_imm(Reg::X0, Reg::X1, 0)); // STRH W0, [X1]
    assert_eq!(last_inst(&e), 0x7900_0020);
    assert!(e.strh_imm(Reg::X0, Reg::X1, 4)); // STRH W0, [X1, #4]
    assert_eq!(last_inst(&e), 0x7900_0820);

    let before = e.code().len();
    assert!(!e.ldrh_imm(Reg::X0, Reg::X1, 3), "odd halfword offset");
    assert!(
        !e.strh_imm(Reg::X0, Reg::X1, 8192),
        "halfword imm12 overflow"
    );
    assert!(!e.strb_imm(Reg::X0, Reg::X1, 4096), "byte imm12 overflow");
    assert_eq!(e.code().len(), before, "a refusal emits nothing");
}

#[test]
fn test_ldp_stp() {
    let mut e = Aarch64Emitter::new();
    // STP X29, X30, [SP, #-16]!
    assert!(e.stp_pre(Reg::X29, Reg::X30, SP, -16));
    let inst = last_inst(&e);
    // opc=10, V=0, encoding=01 (pre-index), L=0
    assert_eq!((inst >> 30) & 0x3, 0b10); // opc
    assert_eq!((inst >> 22) & 1, 0); // L=0 (store)
    assert_eq!(inst & 0x1F, 29); // Rt1 = X29
    assert_eq!((inst >> 10) & 0x1F, 30); // Rt2 = X30

    // LDP X29, X30, [SP], #16
    assert!(e.ldp_post(Reg::X29, Reg::X30, SP, 16));
    let inst = last_inst(&e);
    assert_eq!((inst >> 22) & 1, 1); // L=1 (load)
}

/// A2. The scaled unsigned-offset encoders must REFUSE what they cannot
/// encode exactly, not mask it into a different address.
///
/// `imm12` is 12 bits in units of the access size. The old encoders wrote
/// `imm >> 3` into a field masked with `0xFFF`, so a 16-bit `imm` of 32768
/// scaled to 4096 and came back out as **0** -- a full-page miss, in
/// silence.
#[test]
fn the_scaled_offset_encoders_refuse_what_they_cannot_encode() {
    let mut e = Aarch64Emitter::new();

    // Every exact boundary first: 4095 * 8 for the doubleword form,
    // 4095 * 4 for the word form, 4095 for the byte form. The FP forms
    // share `ldst_unsigned_imm`, so they shared the bug.
    assert!(e.ldr_imm(Reg::X0, Reg::X1, 32760));
    assert!(e.str_imm(Reg::X0, Reg::X1, 32760));
    assert!(e.ldr_imm_w(Reg::X0, Reg::X1, 16380));
    assert!(e.ldrb_imm(Reg::X0, Reg::X1, 4095));
    assert!(e.ldr_fp_d(FpReg::D0, Reg::X1, 32760));
    let before = e.code().len();

    // One unit past it. The old `& 0xFFF` turned this into offset 0.
    assert!(!e.ldr_imm(Reg::X0, Reg::X1, 32768));
    assert!(!e.str_imm(Reg::X0, Reg::X1, 32768));
    // The largest `u16`, which is what made the parameter type the hazard.
    assert!(!e.ldr_imm(Reg::X0, Reg::X1, 65535));
    // Misaligned: `imm >> 3` used to round DOWN and access short.
    assert!(!e.ldr_imm(Reg::X0, Reg::X1, 12));
    assert!(!e.str_imm(Reg::X0, Reg::X1, 4));
    assert!(!e.ldr_imm_w(Reg::X0, Reg::X1, 16384));
    assert!(!e.str_imm_w(Reg::X0, Reg::X1, 2));
    assert!(!e.ldrb_imm(Reg::X0, Reg::X1, 4096));
    assert!(!e.ldr_fp_d(FpReg::D0, Reg::X1, 32768));
    assert!(!e.str_fp_s(FpReg::D0, Reg::X1, 16384));

    assert_eq!(
        e.code().len(),
        before,
        "a refused encoding must emit NOTHING; a half-written instruction \
         is worse than no instruction"
    );
}

/// A3. `ldst_pair`'s `imm7` is SIGNED and scaled by 8, so the byte range is
/// -512..=504. The old `((imm / 8) as u32) & 0x7F` wrapped: `+512` scales
/// to `+64`, whose low seven bits read back as `-64`, i.e. an offset of
/// **-512**. That is a prologue saving registers a kilobyte away from where
/// the epilogue would look for them.
#[test]
fn the_pair_encoders_refuse_an_offset_that_would_wrap_the_signed_imm7() {
    let mut e = Aarch64Emitter::new();

    // Both ends of the real range.
    assert!(e.stp(Reg::X0, Reg::X1, SP, 504));
    assert!(e.stp(Reg::X0, Reg::X1, SP, -512));
    let before = e.code().len();

    // +512 is the value that used to encode as -512.
    assert!(!e.stp(Reg::X0, Reg::X1, SP, 512));
    // +1024 used to encode as 0.
    assert!(!e.stp(Reg::X0, Reg::X1, SP, 1024));
    assert!(!e.ldp(Reg::X0, Reg::X1, SP, -520));
    // Non-8-aligned truncated toward zero: -12 became -8.
    assert!(!e.stp_pre(Reg::X29, Reg::X30, SP, -12));
    assert!(!e.ldp_post(Reg::X29, Reg::X30, SP, 4));

    assert_eq!(e.code().len(), before, "a refused pair must emit nothing");
}

/// The boundary pair encodings, read off the bits.
///
/// `+504` is `+63` scaled, i.e. `0b0111111` -- the largest positive imm7.
/// `-512` is `-64` scaled, i.e. `0b1000000` -- the most negative. Pinning
/// both says the sign handling is real rather than an artefact of the mask.
#[test]
fn the_pair_imm7_field_holds_the_signed_scaled_offset() {
    let mut e = Aarch64Emitter::new();
    assert!(e.stp(Reg::X0, Reg::X1, SP, 504));
    assert_eq!((last_inst(&e) >> 15) & 0x7F, 0b011_1111);

    assert!(e.stp(Reg::X0, Reg::X1, SP, -512));
    assert_eq!((last_inst(&e) >> 15) & 0x7F, 0b100_0000);

    assert!(e.stp_pre(Reg::X29, Reg::X30, SP, -16));
    // -16 / 8 = -2, which is 0b1111110 in seven bits.
    assert_eq!((last_inst(&e) >> 15) & 0x7F, 0b111_1110);
}

// -- Patch --------------------------------------------------------------

#[test]
fn test_patch_branch() {
    let mut e = Aarch64Emitter::new();
    let bpos = e.b(0); // placeholder
    e.nop();
    e.nop();
    let target = e.offset();
    e.patch_branch(bpos, target);

    let inst = inst_at(&e, bpos);
    // Target is 12 bytes from bpos → imm26 = 12/4 = 3
    assert_eq!(inst & 0x03FF_FFFF, 3);
    assert_eq!((inst >> 26) & 0x3F, 0b000101); // still B
}

#[test]
fn test_patch_bcond() {
    let mut e = Aarch64Emitter::new();
    let bpos = e.b_cond(Cond::NE, 0);
    e.nop();
    e.nop();
    e.nop();
    let target = e.offset();
    e.patch_bcond(bpos, target);

    let inst = inst_at(&e, bpos);
    assert_eq!(inst & 0xF, Cond::NE.enc()); // condition preserved
                                            // delta = 16 bytes → imm19 = 4
    assert_eq!((inst >> 5) & 0x7FFFF, 4);
}

// -- FP -----------------------------------------------------------------

#[test]
fn test_fadd_d() {
    let mut e = Aarch64Emitter::new();
    e.fadd_d(FpReg::D0, FpReg::D1, FpReg::D2);
    let inst = last_inst(&e);
    // 0 0 0 11110 01 1 Rm=00010 0010 10 Rn=00001 Rd=00000
    // = 0x1E622820
    assert_eq!(inst, 0x1E62_2820);
}

#[test]
fn test_fsub_fmul_fdiv() {
    let mut e = Aarch64Emitter::new();
    e.fsub_d(FpReg::D3, FpReg::D4, FpReg::D5);
    let inst = last_inst(&e);
    assert_eq!((inst >> 12) & 0xF, 0b0011); // FSUB opcode

    e.fmul_d(FpReg::D6, FpReg::D7, FpReg::D8);
    let inst = last_inst(&e);
    assert_eq!((inst >> 12) & 0xF, 0b0000); // FMUL opcode

    e.fdiv_d(FpReg::D9, FpReg::D10, FpReg::D11);
    let inst = last_inst(&e);
    assert_eq!((inst >> 12) & 0xF, 0b0001); // FDIV opcode
}

#[test]
fn test_fcmp() {
    let mut e = Aarch64Emitter::new();
    e.fcmp_d(FpReg::D0, FpReg::D1);
    let inst = last_inst(&e);
    // 00011110 01 1 00001 00 1000 00000 00 000
    assert_eq!(inst, 0x1E61_2000);
}

#[test]
fn test_scvtf_fcvtzs() {
    let mut e = Aarch64Emitter::new();
    e.scvtf_d_x(FpReg::D0, Reg::X1);
    let inst = last_inst(&e);
    assert_eq!(inst, 0x9E62_0020);

    e.fcvtzs_x_d(Reg::X2, FpReg::D3);
    let inst = last_inst(&e);
    assert_eq!(inst, 0x9E78_0062);
}

// -- A8: the SP / XZR aliasing, as bits ---------------------------------

/// The architecture fact the whole `XZR` contract rests on, pinned as an
/// encoding rather than as a paragraph.
///
/// Both instructions put `0b11111` in `Rn`. They are different instructions:
///
/// * `ADD Xd, Xn, Xm` is data-processing shifted-register
///   (`sf 0 0 01011 shift 0 Rm imm6 Rn Rd`), and **every** register field in
///   that class reads 31 as XZR. So `Rn = 31` is the zero register and the
///   instruction computes `0 + Xm`.
/// * `ADD Xd, Xn, Rm, UXTX #0` is data-processing extended-register
///   (`sf 0 0 01011 00 1 Rm option imm3 Rn Rd`, bit 21 set), and `Rn` and
///   `Rd` in THAT class read 31 as SP. So `Rn = 31` is the stack pointer and
///   the instruction computes `SP + Xm`.
///
/// The two words differ in bit 21 and in the `option`/`imm3` field where the
/// shifted form carries `shift`/`imm6`, and in nothing that names a
/// register. That is why no check on the OPERAND can tell the two cases
/// apart, and why the rule is about which encoder a possibly-SP register is
/// allowed to reach.
#[test]
fn the_same_encoding_is_xzr_in_a_shifted_form_and_sp_in_an_extended_one() {
    // The shifted form can only be GIVEN the zero register for 31 now --
    // `add(X16, SP, X16)` does not compile, see the `compile_fail` doctest
    // on `RegZr`. So the word below is built from `XZR`, which is what that
    // encoding has always actually meant.
    let mut shifted = Aarch64Emitter::new();
    shifted.add(Reg::X16, XZR, Reg::X16);
    let shifted = last_inst(&shifted);

    let mut extended = Aarch64Emitter::new();
    extended.add_ext(Reg::X16, SP, Reg::X16, Extend::UXTX, 0);
    let extended = last_inst(&extended);

    // Rn = 31 in both. Same five bits, two different registers.
    assert_eq!((shifted >> 5) & 0x1F, 31);
    assert_eq!((extended >> 5) & 0x1F, 31);
    assert_ne!(
        shifted, extended,
        "if these were the same word there would be no bug to have"
    );
    // Bit 21 is what separates the classes: 0 = shifted register,
    // 1 = extended register.
    assert_eq!((shifted >> 21) & 1, 0, "shifted-register form");
    assert_eq!((extended >> 21) & 1, 1, "extended-register form");
    // The same five bits -- and, since A8, two different TYPES. Before the
    // split `XZR` WAS `Reg::SP`, so the two calls above were
    // indistinguishable at the argument, which is how both recorded bugs got
    // in. Now each field names which one it reads.
    assert_eq!(XZR.enc(), 31);
    assert_eq!(SP.enc(), 31);
}

/// A load/store base reads 31 as SP, which is the other half of the rule.
///
/// `LDUR Xt, [Xn, #simm9]` is `xx111000010 imm9 00 Rn Rt`; the base field
/// `Rn` is SP-form, so `[SP, #0]` is a real stack access and not a read of
/// address zero.
#[test]
fn a_load_store_base_of_thirty_one_is_the_stack_pointer() {
    let mut e = Aarch64Emitter::new();
    e.ldur(Reg::X0, SP, 0);
    let inst = last_inst(&e);
    assert_eq!((inst >> 5) & 0x1F, 31, "Rn = SP");
    assert_eq!(inst & 0x1F, 0, "Rt = X0");
}

/// The seam the A8 note describes: provenance is invisible at an operand and
/// visible at the point a `Reg` is built from a number.
#[test]
fn the_allocator_facing_constructor_refuses_encoding_thirty_one() {
    // Since A8, `Reg` has no 31 at all, so BOTH `Reg` constructors refuse
    // it. Before, `from_u8(31)` answered `Reg::SP` and only `from_u8_gp`
    // refused; that distinction is now carried by the return type instead.
    assert_eq!(
        Reg::from_u8(31),
        None,
        "31 is not a general-purpose register"
    );
    assert_eq!(
        Reg::from_u8_gp(31),
        None,
        "a 31 arriving through the allocator path is a bug, not a zero operand"
    );
    // The frame-layout path, which legitimately means SP, asks `RegSp`.
    assert_eq!(RegSp::from_u8(31), Some(SP));
    // Everything else is unchanged, including the boundaries, and the two
    // constructors agree on every general-purpose register.
    for n in 0..=30u8 {
        assert_eq!(Reg::from_u8_gp(n), Reg::from_u8(n), "X{n}");
        assert_eq!(RegSp::from_u8(n), Reg::from_u8(n).map(RegSp::X), "X{n}");
    }
    assert_eq!(Reg::from_u8_gp(32), None);
    assert_eq!(RegSp::from_u8(32), None);
    // No `Reg` encodes as 31: the whole point of the split.
    for n in 0..=30u8 {
        assert_ne!(Reg::from_u8(n).expect("X0..X30").enc(), 31);
    }
}

// -- System -------------------------------------------------------------

#[test]
fn test_nop() {
    let mut e = Aarch64Emitter::new();
    e.nop();
    assert_eq!(last_inst(&e), 0xD503_201F);
}

#[test]
fn test_brk() {
    let mut e = Aarch64Emitter::new();
    e.brk(0);
    assert_eq!(last_inst(&e), 0xD420_0000);
    e.brk(1);
    assert_eq!(last_inst(&e), 0xD420_0020);
}

#[test]
fn test_ret_lr() {
    let mut e = Aarch64Emitter::new();
    e.ret_lr();
    // RET X30 = 0xD65F03C0
    assert_eq!(last_inst(&e), 0xD65F_03C0);
}

// -- CMP / MOV ----------------------------------------------------------

#[test]
fn test_cmp() {
    let mut e = Aarch64Emitter::new();
    e.cmp(Reg::X1, Reg::X2);
    // SUBS XZR, X1, X2
    let inst = last_inst(&e);
    assert_eq!(inst & 0x1F, 31); // Rd = XZR
    assert_eq!((inst >> 5) & 0x1F, 1); // Rn = X1
    assert_eq!((inst >> 16) & 0x1F, 2); // Rm = X2
}

#[test]
fn test_mov_reg() {
    let mut e = Aarch64Emitter::new();
    e.mov(Reg::X0, Reg::X1);
    // ORR X0, XZR, X1
    let inst = last_inst(&e);
    assert_eq!(inst & 0x1F, 0); // Rd = X0
    assert_eq!((inst >> 5) & 0x1F, 31); // Rn = XZR
    assert_eq!((inst >> 16) & 0x1F, 1); // Rm = X1
}

// -- Comprehensive mov_imm64 patterns -----------------------------------

#[test]
fn test_mov_imm64_zero() {
    let mut e = Aarch64Emitter::new();
    e.mov_imm64(Reg::X0, 0);
    assert_eq!(e.code().len(), 4); // single MOVZ
    let inst = inst_at(&e, 0);
    assert_eq!((inst >> 29) & 0x3, 0b10); // MOVZ
    assert_eq!((inst >> 5) & 0xFFFF, 0);
}

#[test]
fn test_mov_imm64_negative_one_pattern() {
    // 0xFFFFFFFF_FFFF0000 has chunks [0x0000, 0xFFFF, 0xFFFF, 0xFFFF]
    // Three 0xFFFF chunks → MOVN strategy better.
    let mut e = Aarch64Emitter::new();
    e.mov_imm64(Reg::X5, 0xFFFF_FFFF_FFFF_0000);
    // Should use MOVN with chunk0's NOT, then no MOVKs needed since all
    // other chunks are 0xFFFF.
    // MOVN X5, #0xFFFF, LSL#0 → sets bits 0..15 to NOT(0xFFFF) = 0x0000, rest to 1
    // Actually: MOVN X5, #0, LSL#0 → NOT(0) = 0xFFFF_FFFF_FFFF_FFFF, but
    // we need 0xFFFF_FFFF_FFFF_0000. So MOVN X5, #0xFFFF, LSL#0.
    // NOT of (0x000000000000FFFF) = 0xFFFFFFFFFFFF0000. Correct!
    assert_eq!(e.code().len(), 4); // single instruction
}

// -- Branch-offset range checks ----------------------------------------

#[test]
fn test_imm_fits_boundaries() {
    // imm26 (B/BL): ±128 MB, 4-aligned.
    assert!(imm26_fits(0));
    assert!(imm26_fits((1 << 27) - 4));
    assert!(imm26_fits(-(1 << 27)));
    assert!(!imm26_fits(1 << 27)); // just out of range
    assert!(!imm26_fits(-(1 << 27) - 4));
    assert!(!imm26_fits(2)); // not 4-aligned

    // imm19 (B.cond/CBZ/CBNZ/LDR-literal): ±1 MB.
    assert!(imm19_fits((1 << 20) - 4));
    assert!(imm19_fits(-(1 << 20)));
    assert!(!imm19_fits(1 << 20));
    assert!(!imm19_fits(-(1 << 20) - 4));
    assert!(!imm19_fits(1)); // not 4-aligned

    // imm14 (TBZ/TBNZ): ±32 KB.
    assert!(imm14_fits((1 << 15) - 4));
    assert!(imm14_fits(-(1 << 15)));
    assert!(!imm14_fits(1 << 15));
    assert!(!imm14_fits(-(1 << 15) - 4));

    // imm21 (ADR): 21-bit signed, unscaled.
    assert!(imm21_fits((1 << 20) - 1));
    assert!(imm21_fits(-(1 << 20)));
    assert!(!imm21_fits(1 << 20));
    assert!(!imm21_fits(-(1 << 20) - 1));
}

#[test]
fn test_in_range_branch_does_not_overflow() {
    let mut e = Aarch64Emitter::new();
    e.b(0x100);
    e.b_cond(Cond::EQ, -0x40);
    e.tbz(Reg::X0, 5, 8);
    e.cbz(Reg::X3, (1 << 20) - 4);
    assert!(
        !e.overflowed(),
        "in-range branches must not set the overflow flag"
    );
}

#[test]
fn test_in_range_patch_does_not_overflow() {
    let mut e = Aarch64Emitter::new();
    let p = e.b(0);
    // Patch to a nearby in-range target.
    e.patch_branch(p, p + 0x20);
    assert!(!e.overflowed());
}

// `mark_branch_overflow` does two things on an out-of-range branch: it
// trips a `debug_assert!` (loud panic in debug/test builds) AND sets the
// sticky `overflowed` flag (always) so release builds discard the buffer
// and bail to the interpreter. `debug_assert!` is compiled OUT under
// `--release`, so a release test run sees no panic — these tests must
// check the mechanism that is actually active for the build: the panic in
// debug, the sticky flag in release. (Previously they only asserted the
// panic and so spuriously FAILED under `cargo test --release`.)
fn assert_branch_overflow_detected<F>(emit: F)
where
    F: Fn(&mut Aarch64Emitter),
{
    if cfg!(debug_assertions) {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the panic message
        let emit_ref = &emit;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut e = Aarch64Emitter::new();
            emit_ref(&mut e);
        }));
        std::panic::set_hook(prev);
        assert!(
            result.is_err(),
            "expected an out-of-range branch offset to trip debug_assert!"
        );
    } else {
        let mut e = Aarch64Emitter::new();
        emit(&mut e);
        assert!(
            e.overflowed(),
            "expected an out-of-range branch offset to set the sticky overflowed flag"
        );
    }
}

#[test]
fn test_b_cond_beyond_1mb_overflows() {
    // > ±1 MB conditional branch must not silently truncate.
    assert_branch_overflow_detected(|e| {
        e.b_cond(Cond::NE, 1 << 20);
    });
}

#[test]
fn test_tbz_beyond_32kb_overflows() {
    // > ±32 KB test-bit branch must not silently truncate.
    assert_branch_overflow_detected(|e| {
        e.tbz(Reg::X0, 3, 1 << 15);
    });
}

#[test]
fn test_b_beyond_128mb_overflows() {
    assert_branch_overflow_detected(|e| {
        e.b(1 << 27);
    });
}

#[test]
fn test_patch_branch_out_of_range_overflows() {
    assert_branch_overflow_detected(|e| {
        let p = e.b(0);
        e.patch_branch(p, p + (1 << 27));
    });
}

// -- 2026-09-12 review fixes --------------------------------------------

/// A zero low halfword costs no instruction. The old MOVZ path always
/// MOVZ'd halfword 0, so `0x1_0000` took two words.
#[test]
fn test_mov_imm64_skips_zero_halfwords() {
    let mut e = Aarch64Emitter::new();
    e.mov_imm64(Reg::X3, 0x0001_0000);
    assert_eq!(e.code().len(), 4, "one MOVZ #1, LSL #16");
    let i = inst_at(&e, 0);
    assert_eq!((i >> 29) & 0x3, 0b10, "MOVZ");
    assert_eq!((i >> 21) & 0x3, 1, "hw = 1 (LSL #16)");
    assert_eq!((i >> 5) & 0xFFFF, 1);

    let mut e2 = Aarch64Emitter::new();
    e2.mov_imm64(Reg::X0, 0x0000_ABCD_0000_0000);
    assert_eq!(e2.code().len(), 4, "a lone halfword at LSL #32 is one word");
    assert_eq!((inst_at(&e2, 0) >> 21) & 0x3, 2);

    let mut e3 = Aarch64Emitter::new();
    e3.mov_imm64(Reg::X0, 0x1234_0000_0000_5678);
    assert_eq!(
        e3.code().len(),
        8,
        "MOVZ #0x5678 then MOVK #0x1234, LSL #48"
    );
}

/// Patching a TBZ must not destroy its bit number.
///
/// TBZ's bits 23:19 are the low five bits of the TESTED BIT, and the old
/// `patch_bcond` cleared bits 23:5 for every word it touched.
#[test]
fn test_patch_bcond_preserves_a_tbz_bit_number() {
    let mut e = Aarch64Emitter::new();
    let pos = e.tbz(Reg::X3, 37, 0);
    e.nop();
    e.nop();
    let target = e.offset();
    e.patch_bcond(pos, target);
    let inst = inst_at(&e, pos);
    assert_eq!((inst >> 31) & 1, 1, "b5 of bit 37 survives");
    assert_eq!((inst >> 19) & 0x1F, 37 & 0x1F, "b40 of bit 37 survives");
    assert_eq!((inst >> 5) & 0x3FFF, 3, "imm14 = 12 bytes / 4");
    assert_eq!(inst & 0x1F, 3, "Rt survives");
    assert!(!e.overflowed());

    // CBZ still takes the imm19 path.
    let mut c = Aarch64Emitter::new();
    let cpos = c.cbz(Reg::X9, 0);
    c.nop();
    let ctarget = c.offset();
    c.patch_bcond(cpos, ctarget);
    let ci = inst_at(&c, cpos);
    assert_eq!((ci >> 5) & 0x7FFFF, 2);
    assert_eq!(ci & 0x1F, 9);
    assert_eq!((ci >> 24) & 0xFF, 0xB4, "still CBZ");
}

/// A TBZ patch beyond its ±32 KB field overflows, rather than being
/// checked against the wider imm19 range.
#[test]
fn test_patch_bcond_tbz_out_of_range_overflows() {
    assert_branch_overflow_detected(|e| {
        let p = e.tbz(Reg::X0, 1, 0);
        e.patch_bcond(p, p + (1 << 15));
    });
}

/// Bitmask immediates, against words produced by an assembler.
#[test]
fn test_logical_immediate_encodings() {
    let mut e = Aarch64Emitter::new();
    assert!(e.and_imm(Reg::X0, Reg::X1, 0xFFFF));
    assert_eq!(last_inst(&e), 0x9240_3C20, "and x0, x1, #0xffff");
    assert!(e.and_imm_w(Reg::X0, Reg::X1, 0xFF));
    assert_eq!(last_inst(&e), 0x1200_1C20, "and w0, w1, #0xff");
    assert!(e.and_imm(Reg::X0, Reg::X0, 0x5555_5555_5555_5555));
    assert_eq!(
        last_inst(&e),
        0x9200_F000,
        "and x0, x0, #0x5555555555555555"
    );
    assert!(e.and_imm(Reg::X2, Reg::X2, 31));
    assert_eq!(last_inst(&e), 0x9240_1042, "and x2, x2, #31");
    assert!(e.orr_imm(Reg::X0, Reg::X0, 0xFFFF_FFFF_0000_0000));
    assert_eq!(
        encode_logical_imm(0xFFFF_FFFF_0000_0000, 64),
        Some((1, 32, 31)),
        "a rotated run of 32 ones"
    );

    // Not encodable: nothing is emitted and the caller is told.
    let before = e.code().len();
    assert!(!e.and_imm(Reg::X0, Reg::X0, 0), "all-zeros");
    assert!(!e.and_imm(Reg::X0, Reg::X0, u64::MAX), "all-ones");
    assert!(!e.and_imm(Reg::X0, Reg::X0, 0b101), "not a rotated run");
    assert!(!e.and_imm_w(Reg::X0, Reg::X0, u32::MAX), "32-bit all-ones");
    assert_eq!(e.code().len(), before, "a refused immediate emits nothing");
    assert_eq!(encode_logical_imm(0x1_0000_0000, 32), None, "wider than W");
}

/// CSET/CSETM/CNEG and the condition inversion they rely on.
#[test]
fn test_conditional_select_encodings() {
    let mut e = Aarch64Emitter::new();
    e.cset(Reg::X0, Cond::EQ);
    assert_eq!(
        last_inst(&e),
        0x9A9F_17E0,
        "cset x0, eq = csinc x0, xzr, xzr, ne"
    );
    e.csetm(Reg::X1, Cond::LT);
    assert_eq!(
        last_inst(&e),
        0xDA9F_A3E1,
        "csetm x1, lt = csinv x1, xzr, xzr, ge"
    );
    e.cneg(Reg::X0, Reg::X0, Cond::LT);
    assert_eq!(
        last_inst(&e),
        0xDA80_A400,
        "cneg x0, x0, lt = csneg x0, x0, x0, ge"
    );
    e.csel(Reg::X2, Reg::X3, Reg::X4, Cond::HI);
    assert_eq!(last_inst(&e), 0x9A84_8062, "csel x2, x3, x4, hi");

    for c in [
        Cond::EQ,
        Cond::HS,
        Cond::MI,
        Cond::VS,
        Cond::HI,
        Cond::GE,
        Cond::GT,
        Cond::AL,
    ] {
        assert_eq!(c.invert().invert(), c);
        assert_eq!(
            c.invert().enc(),
            c.enc() ^ 1,
            "{c:?} inverts by flipping bit 0"
        );
    }
}

#[test]
fn test_sign_and_zero_extension_encodings() {
    let mut e = Aarch64Emitter::new();
    e.sxtw(Reg::X0, Reg::X1);
    assert_eq!(last_inst(&e), 0x9340_7C20, "sxtw x0, w1");
    e.sxtb(Reg::X2, Reg::X3);
    assert_eq!(last_inst(&e), 0x9340_1C62, "sxtb x2, w3");
    e.sxth(Reg::X2, Reg::X3);
    assert_eq!(last_inst(&e), 0x9340_3C62, "sxth x2, w3");
    e.uxtw(Reg::X0, Reg::X1);
    assert_eq!(last_inst(&e), 0x2A01_03E0, "mov w0, w1");
}

/// The extended-register form is the one ADD in which register 31 is SP on
/// both sides -- which is what makes `SP +/- X16` expressible at all.
#[test]
fn test_extended_register_addsub_encodings() {
    let mut e = Aarch64Emitter::new();
    e.add_ext(SP, SP, Reg::X16, Extend::UXTX, 0);
    assert_eq!(last_inst(&e), 0x8B30_63FF, "add sp, sp, x16");
    e.sub_ext(SP, SP, Reg::X16, Extend::UXTX, 0);
    assert_eq!(last_inst(&e), 0xCB30_63FF, "sub sp, sp, x16");
    e.add_ext(Reg::X16, Reg::X29, Reg::X16, Extend::UXTX, 0);
    assert_eq!(last_inst(&e), 0x8B30_63B0, "add x16, x29, x16");
    // The shifted-register ADD with Rn = 31 is a DIFFERENT instruction
    // (it reads XZR) -- the control that makes the words above mean
    // something.
    let mut s = Aarch64Emitter::new();
    s.add(Reg::X16, XZR, Reg::X16);
    assert_ne!(
        last_inst(&s),
        0x8B30_63F0,
        "shifted-register ADD is not SP-relative"
    );
}

#[test]
fn test_w_form_data_processing_encodings() {
    let mut e = Aarch64Emitter::new();
    e.lsl_w(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x1AC2_2020, "lsl w0, w1, w2");
    e.lsr_w(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x1AC2_2420, "lsr w0, w1, w2");
    e.asr_w(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x1AC2_2820, "asr w0, w1, w2");
    e.cmp_w(Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x6B02_003F, "cmp w1, w2");
    e.cmp_imm_w(Reg::X0, 1);
    assert_eq!(last_inst(&e), 0x7100_041F, "cmp w0, #1");
    e.cmn_imm_w(Reg::X0, 1);
    assert_eq!(last_inst(&e), 0x3100_041F, "cmn w0, #1");
    e.neg_w(Reg::X0, Reg::X1);
    assert_eq!(last_inst(&e), 0x4B01_03E0, "neg w0, w1");
    e.and_w(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x0A02_0020, "and w0, w1, w2");
    e.mul_w(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x1B02_7C20, "mul w0, w1, w2");
}

#[test]
fn test_fp_width_conversion_encodings() {
    let mut e = Aarch64Emitter::new();
    e.fmov_s_from_w(FpReg::D0, Reg::X1);
    assert_eq!(last_inst(&e), 0x1E27_0020, "fmov s0, w1");
    e.fmov_w_from_s(Reg::X0, FpReg::D1);
    assert_eq!(last_inst(&e), 0x1E26_0020, "fmov w0, s1");
    e.scvtf_s_x(FpReg::D0, Reg::X1);
    assert_eq!(last_inst(&e), 0x9E22_0020, "scvtf s0, x1");
    e.fcvtzs_w_d(Reg::X0, FpReg::D1);
    assert_eq!(last_inst(&e), 0x1E78_0020, "fcvtzs w0, d1");
    e.fcvtzs_x_s(Reg::X0, FpReg::D1);
    assert_eq!(last_inst(&e), 0x9E38_0020, "fcvtzs x0, s1");
    e.fcvtzs_w_s(Reg::X0, FpReg::D1);
    assert_eq!(last_inst(&e), 0x1E38_0020, "fcvtzs w0, s1");
    e.fneg_s(FpReg::D0, FpReg::D1);
    assert_eq!(last_inst(&e), 0x1E21_4020, "fneg s0, s1");
    e.fneg_d(FpReg::D0, FpReg::D1);
    assert_eq!(last_inst(&e), 0x1E61_4020, "fneg d0, d1");
    e.fcmp_s(FpReg::D0, FpReg::D1);
    assert_eq!(last_inst(&e), 0x1E21_2000, "fcmp s0, s1");
}

#[test]
fn test_ldrsw_register_offset_encoding() {
    let mut e = Aarch64Emitter::new();
    e.ldrsw_reg_uxtw_scaled(Reg::X17, Reg::X16, Reg::X17);
    assert_eq!(last_inst(&e), 0xB8B1_5A11, "ldrsw x17, [x16, w17, uxtw #2]");
}

// -- R10 (arm lane): exact-word tests for encoders with production call
// sites in `aarch64_backend.rs` (the `Fcvt*`, `Fmov*GpFromD`/`FmovDFromGp`,
// and NEON `LD1`/`ST1`/`ADD`/`MUL` pseudo-op arms) that had no encoding
// test pinning them against the ARM ARM. Registers are chosen distinct
// (Rd/Rn/Rm all different) so a swapped or mis-shifted field would fail.

#[test]
fn r10arm_fcvt_and_fp_gp_move_encodings() {
    let mut e = Aarch64Emitter::new();

    // FCVT Dd, Sn: single -> double. Base word (all-zero regs) 0x1E22C000
    // is the ARM ARM's fixed encoding for FCVT with ftype=00 (source
    // single), opc=01 (dest double); Rn at bits9:5, Rd at bits4:0.
    e.fcvt_d_s(FpReg::D5, FpReg::D3);
    assert_eq!(last_inst(&e), 0x1E22_C065, "fcvt d5, s3");

    // FCVT Sd, Dn: double -> single (ftype=01, opc=00).
    e.fcvt_s_d(FpReg::D5, FpReg::D3);
    assert_eq!(last_inst(&e), 0x1E62_4065, "fcvt s5, d3");

    // FMOV Dd, Xn: GP -> FP, 64-bit bit-pattern move.
    e.fmov_d_from_gp(FpReg::D5, Reg::X3);
    assert_eq!(last_inst(&e), 0x9E67_0065, "fmov d5, x3");

    // FMOV Xd, Dn: FP -> GP, the converse. Rd (GP) is bits4:0 here, Rn
    // (FP) stays at bits9:5 -- the same field positions as every other
    // FP<->GP move, which is exactly what would break if Rd and Rn were
    // swapped between the two directions.
    e.fmov_gp_from_d(Reg::X5, FpReg::D3);
    assert_eq!(last_inst(&e), 0x9E66_0065, "fmov x5, d3");
}

#[test]
fn r10arm_neon_ld1_st1_and_vector_alu_encodings() {
    let mut e = Aarch64Emitter::new();

    // LD1 {Vt.4S}, [Xn]: base 0x4C407800 (Q=1, L=1, opcode=0111, size=10)
    // with Rn at bits9:5 and Rt at bits4:0.
    e.ld1_4s(FpReg::D5, Reg::X3);
    assert_eq!(last_inst(&e), 0x4C40_7865, "ld1 {{v5.4s}}, [x3]");

    // ST1 {Vt.4S}, [Xn]: same shape, L=0.
    e.st1_4s(FpReg::D5, Reg::X3);
    assert_eq!(last_inst(&e), 0x4C00_7865, "st1 {{v5.4s}}, [x3]");

    // ADD Vd.4S, Vn.4S, Vm.4S: base 0x4EA08400 (Q=1, U=0, size=10,
    // opcode=10000); Rm at bits20:16, Rn at bits9:5, Rd at bits4:0 --
    // three distinct registers exercise all three fields independently.
    e.add_v4s(FpReg::D5, FpReg::D3, FpReg::D7);
    assert_eq!(last_inst(&e), 0x4EA7_8465, "add v5.4s, v3.4s, v7.4s");

    // MUL Vd.4S, Vn.4S, Vm.4S: same shape, opcode=10011.
    e.mul_v4s(FpReg::D5, FpReg::D3, FpReg::D7);
    assert_eq!(last_inst(&e), 0x4EA7_9C65, "mul v5.4s, v3.4s, v7.4s");
}

/// `adr`, `ldr_literal_x` and their patchers -- the literal-pool/address
/// machinery `aarch64_backend.rs` uses for `tableswitch`'s jump-table base
/// and had no exact-word coverage of its own (only the overflow paths were
/// pinned, by `test_in_range_patch_does_not_overflow` and friends).
#[test]
fn r10arm_adr_and_literal_pool_encodings() {
    let mut e = Aarch64Emitter::new();

    // ADR Xd, #6: unscaled byte offset 6 = 0b110 splits as immlo=2 (bits
    // 30:29), immhi=1 (bits 23:5). Rd=X3 must land at bits4:0 only.
    e.adr(Reg::X3, 6);
    assert_eq!(last_inst(&e), 0x5000_0023, "adr x3, #6");

    // LDR Xt, <literal>: opc=01, V=0, fixed 011000, imm19 placeholder 0.
    e.ldr_literal_x(Reg::X5);
    assert_eq!(last_inst(&e), 0x5800_0005, "ldr x5, <literal>");

    // patch_adr: a zero-offset placeholder for X0, followed by two NOPs
    // and then patched to that point (12 bytes ahead, matching
    // `test_patch_branch`'s pattern). 12 = 0b1100 -> immlo=0, immhi=3, so
    // only bits9:5 change from the placeholder's all-zero immediate field.
    let mut pe = Aarch64Emitter::new();
    let adr_pos = pe.load_label(Reg::X0);
    assert_eq!(inst_at(&pe, adr_pos), 0x1000_0000, "placeholder ADR x0, #0");
    pe.nop();
    pe.nop();
    let adr_target = pe.offset();
    pe.patch_adr(adr_pos, adr_target);
    assert_eq!(
        inst_at(&pe, adr_pos),
        0x1000_0060,
        "adr x0, #12 after patch"
    );

    // patch_ldr_literal: same idea for the literal-load placeholder.
    let mut le = Aarch64Emitter::new();
    let ldr_pos = le.ldr_literal_x(Reg::X0);
    assert_eq!(
        inst_at(&le, ldr_pos),
        0x5800_0000,
        "placeholder LDR x0, <literal>"
    );
    le.nop();
    le.nop();
    let ldr_target = le.offset();
    le.patch_ldr_literal(ldr_pos, ldr_target);
    assert_eq!(
        inst_at(&le, ldr_pos),
        0x5800_0060,
        "ldr x0, <literal> after patch, imm19=3"
    );
}

/// `bic`/`udiv` and `eor_imm`: public emitter API with no production call
/// site today (the backend never emits `BIC` or unsigned division, and
/// `EOR`/`imm` has no caller either), but still part of the encoder's
/// contract and worth pinning like every sibling in the same families.
#[test]
fn r10arm_bic_udiv_and_eor_imm_encodings() {
    let mut e = Aarch64Emitter::new();

    // BIC Xd, Xn, Xm = AND with N=1. Differs from `and(X0, X1, X2)`
    // (0x8A02_0020, see `test_and_orr_eor`) only in bit21.
    e.bic(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x8A22_0020, "bic x0, x1, x2");

    // UDIV Xd, Xn, Xm: same shape as `test_sdiv`'s SDIV (0x9AC2_0C20),
    // opcode 0b000010 instead of 0b000011 -- bit10 is the only bit that
    // differs (0x0C -> 0x08 in the third byte).
    e.udiv(Reg::X0, Reg::X1, Reg::X2);
    assert_eq!(last_inst(&e), 0x9AC2_0820, "udiv x0, x1, x2");

    // EOR Xd, Xn, #0xffff: opc=10, vs. AND's opc=00 for the identical
    // immediate in `test_logical_immediate_encodings`
    // (`and_imm(X0, X1, 0xffff)` = 0x9240_3C20). Only bit30 differs.
    assert!(e.eor_imm(Reg::X0, Reg::X1, 0xFFFF));
    assert_eq!(last_inst(&e), 0xD240_3C20, "eor x0, x1, #0xffff");
}
