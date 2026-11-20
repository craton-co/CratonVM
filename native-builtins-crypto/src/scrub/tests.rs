use super::*;

extern "C" fn scrub_c() {
    scrub_vector_registers();
}

#[test]
fn scrub_clears_xmm1() {
    let mut buf = [0xA5u8; 16];
    unsafe {
        core::arch::asm!(
            "movdqu xmm1, [r12]",
            "sub rsp, 32",
            "call {f}",
            "add rsp, 32",
            "movdqu [r12], xmm1",
            f = sym scrub_c,
            in("r12") buf.as_mut_ptr(),
            clobber_abi("C"),
        );
    }
    assert_eq!(buf, [0u8; 16]);
}

#[test]
fn scrub_clears_zmm31_and_k7() {
    if !std::is_x86_feature_detected!("avx512f") {
        return;
    }
    let mut buf = [0xA5u8; 64];
    let mut mask: u16 = 0xFFFF;
    unsafe { load_call_store_avx512(buf.as_mut_ptr(), &mut mask) };
    assert_eq!(buf, [0u8; 64]);
    assert_eq!(mask, 0);
}

#[target_feature(enable = "avx512f")]
unsafe fn load_call_store_avx512(buf: *mut u8, mask: *mut u16) {
    unsafe {
        core::arch::asm!(
            "vmovdqu64 zmm31, [r12]",
            "kmovw k7, word ptr [r13]",
            "sub rsp, 32",
            "call {f}",
            "add rsp, 32",
            "vmovdqu64 [r12], zmm31",
            "kmovw word ptr [r13], k7",
            f = sym scrub_c,
            in("r12") buf,
            in("r13") mask,
            clobber_abi("C"),
        );
    }
}

#[test]
fn scrub_clears_all_32_zmm_registers_and_all_mask_registers() {
    if !std::is_x86_feature_detected!("avx512f") {
        return;
    }
    let mut buf = [0xA5u8; 8 * 64];
    let mut mask: [u16; 2] = [0xFFFF, 0xFFFF];

    unsafe {
        load_call_store_bank0(buf.as_mut_ptr(), mask.as_mut_ptr());
        assert_eq!(buf, [0u8; 8 * 64], "zmm0-zmm7 must be zeroed");
        assert_eq!(mask, [0u16; 2], "k0-k1 must be zeroed");

        buf.fill(0x5Au8);
        mask.fill(0xAAAA);
        load_call_store_bank1(buf.as_mut_ptr(), mask.as_mut_ptr());
        assert_eq!(buf, [0u8; 8 * 64], "zmm8-zmm15 must be zeroed");
        assert_eq!(mask, [0u16; 2], "k2-k3 must be zeroed");

        buf.fill(0x3Cu8);
        mask.fill(0x5555);
        load_call_store_bank2(buf.as_mut_ptr(), mask.as_mut_ptr());
        assert_eq!(buf, [0u8; 8 * 64], "zmm16-zmm23 must be zeroed");
        assert_eq!(mask, [0u16; 2], "k4-k5 must be zeroed");

        buf.fill(0xF0u8);
        mask.fill(0xFFFF);
        load_call_store_bank3(buf.as_mut_ptr(), mask.as_mut_ptr());
        assert_eq!(buf, [0u8; 8 * 64], "zmm24-zmm31 must be zeroed");
        assert_eq!(mask, [0u16; 2], "k6-k7 must be zeroed");
    }
}

#[target_feature(enable = "avx512f")]
unsafe fn load_call_store_bank0(buf: *mut u8, mask: *mut u16) {
    unsafe {
        core::arch::asm!(
            "vmovdqu64 zmm0, [r12 + 0*64]",
            "vmovdqu64 zmm1, [r12 + 1*64]",
            "vmovdqu64 zmm2, [r12 + 2*64]",
            "vmovdqu64 zmm3, [r12 + 3*64]",
            "vmovdqu64 zmm4, [r12 + 4*64]",
            "vmovdqu64 zmm5, [r12 + 5*64]",
            "vmovdqu64 zmm6, [r12 + 6*64]",
            "vmovdqu64 zmm7, [r12 + 7*64]",
            "kmovw k0, word ptr [r13 + 0*2]",
            "kmovw k1, word ptr [r13 + 1*2]",
            "sub rsp, 32",
            "call {f}",
            "add rsp, 32",
            "vmovdqu64 [r12 + 0*64], zmm0",
            "vmovdqu64 [r12 + 1*64], zmm1",
            "vmovdqu64 [r12 + 2*64], zmm2",
            "vmovdqu64 [r12 + 3*64], zmm3",
            "vmovdqu64 [r12 + 4*64], zmm4",
            "vmovdqu64 [r12 + 5*64], zmm5",
            "vmovdqu64 [r12 + 6*64], zmm6",
            "vmovdqu64 [r12 + 7*64], zmm7",
            "kmovw word ptr [r13 + 0*2], k0",
            "kmovw word ptr [r13 + 1*2], k1",
            f = sym scrub_c,
            in("r12") buf,
            in("r13") mask,
            clobber_abi("C"),
        );
    }
}

#[target_feature(enable = "avx512f")]
unsafe fn load_call_store_bank1(buf: *mut u8, mask: *mut u16) {
    unsafe {
        core::arch::asm!(
            "vmovdqu64 zmm8, [r12 + 0*64]",
            "vmovdqu64 zmm9, [r12 + 1*64]",
            "vmovdqu64 zmm10, [r12 + 2*64]",
            "vmovdqu64 zmm11, [r12 + 3*64]",
            "vmovdqu64 zmm12, [r12 + 4*64]",
            "vmovdqu64 zmm13, [r12 + 5*64]",
            "vmovdqu64 zmm14, [r12 + 6*64]",
            "vmovdqu64 zmm15, [r12 + 7*64]",
            "kmovw k2, word ptr [r13 + 0*2]",
            "kmovw k3, word ptr [r13 + 1*2]",
            "sub rsp, 32",
            "call {f}",
            "add rsp, 32",
            "vmovdqu64 [r12 + 0*64], zmm8",
            "vmovdqu64 [r12 + 1*64], zmm9",
            "vmovdqu64 [r12 + 2*64], zmm10",
            "vmovdqu64 [r12 + 3*64], zmm11",
            "vmovdqu64 [r12 + 4*64], zmm12",
            "vmovdqu64 [r12 + 5*64], zmm13",
            "vmovdqu64 [r12 + 6*64], zmm14",
            "vmovdqu64 [r12 + 7*64], zmm15",
            "kmovw word ptr [r13 + 0*2], k2",
            "kmovw word ptr [r13 + 1*2], k3",
            f = sym scrub_c,
            in("r12") buf,
            in("r13") mask,
            clobber_abi("C"),
        );
    }
}

#[target_feature(enable = "avx512f")]
unsafe fn load_call_store_bank2(buf: *mut u8, mask: *mut u16) {
    unsafe {
        core::arch::asm!(
            "vmovdqu64 zmm16, [r12 + 0*64]",
            "vmovdqu64 zmm17, [r12 + 1*64]",
            "vmovdqu64 zmm18, [r12 + 2*64]",
            "vmovdqu64 zmm19, [r12 + 3*64]",
            "vmovdqu64 zmm20, [r12 + 4*64]",
            "vmovdqu64 zmm21, [r12 + 5*64]",
            "vmovdqu64 zmm22, [r12 + 6*64]",
            "vmovdqu64 zmm23, [r12 + 7*64]",
            "kmovw k4, word ptr [r13 + 0*2]",
            "kmovw k5, word ptr [r13 + 1*2]",
            "sub rsp, 32",
            "call {f}",
            "add rsp, 32",
            "vmovdqu64 [r12 + 0*64], zmm16",
            "vmovdqu64 [r12 + 1*64], zmm17",
            "vmovdqu64 [r12 + 2*64], zmm18",
            "vmovdqu64 [r12 + 3*64], zmm19",
            "vmovdqu64 [r12 + 4*64], zmm20",
            "vmovdqu64 [r12 + 5*64], zmm21",
            "vmovdqu64 [r12 + 6*64], zmm22",
            "vmovdqu64 [r12 + 7*64], zmm23",
            "kmovw word ptr [r13 + 0*2], k4",
            "kmovw word ptr [r13 + 1*2], k5",
            f = sym scrub_c,
            in("r12") buf,
            in("r13") mask,
            clobber_abi("C"),
        );
    }
}

#[target_feature(enable = "avx512f")]
unsafe fn load_call_store_bank3(buf: *mut u8, mask: *mut u16) {
    unsafe {
        core::arch::asm!(
            "vmovdqu64 zmm24, [r12 + 0*64]",
            "vmovdqu64 zmm25, [r12 + 1*64]",
            "vmovdqu64 zmm26, [r12 + 2*64]",
            "vmovdqu64 zmm27, [r12 + 3*64]",
            "vmovdqu64 zmm28, [r12 + 4*64]",
            "vmovdqu64 zmm29, [r12 + 5*64]",
            "vmovdqu64 zmm30, [r12 + 6*64]",
            "vmovdqu64 zmm31, [r12 + 7*64]",
            "kmovw k6, word ptr [r13 + 0*2]",
            "kmovw k7, word ptr [r13 + 1*2]",
            "sub rsp, 32",
            "call {f}",
            "add rsp, 32",
            "vmovdqu64 [r12 + 0*64], zmm24",
            "vmovdqu64 [r12 + 1*64], zmm25",
            "vmovdqu64 [r12 + 2*64], zmm26",
            "vmovdqu64 [r12 + 3*64], zmm27",
            "vmovdqu64 [r12 + 4*64], zmm28",
            "vmovdqu64 [r12 + 5*64], zmm29",
            "vmovdqu64 [r12 + 6*64], zmm30",
            "vmovdqu64 [r12 + 7*64], zmm31",
            "kmovw word ptr [r13 + 0*2], k6",
            "kmovw word ptr [r13 + 1*2], k7",
            f = sym scrub_c,
            in("r12") buf,
            in("r13") mask,
            clobber_abi("C"),
        );
    }
}
